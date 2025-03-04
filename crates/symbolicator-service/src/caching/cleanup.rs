use std::ffi::OsStr;
use std::fs::{read_dir, remove_dir_all, remove_file};
use std::io;
use std::path::Path;

use anyhow::{anyhow, Result};
use rand::seq::SliceRandom;
use rand::thread_rng;
use rayon::prelude::*;

use crate::caching::fs::{metadata_path, METADATA_EXTENSION};
use crate::config::Config;
use crate::metric;

use super::fs::catch_not_found;
use super::{Cache, Caches};

/// Entry function for the cleanup command.
///
/// This will clean up all caches based on configured cache retention.
/// If `dry_run` is `true`, no files will actually be deleted.
pub fn cleanup(config: Config, dry_run: bool) -> Result<()> {
    Caches::from_config(&config)?.cleanup(dry_run)
}

impl Caches {
    /// Clear the temporary files.
    ///
    /// We need to do this on startup of the main symbolicator process to avoid accidentally
    /// leaving temporary files which survive a hard crash.
    pub fn clear_tmp(&self, config: &Config) -> io::Result<()> {
        if let Some(ref tmp) = config.cache_dir("tmp") {
            if tmp.exists() {
                std::fs::remove_dir_all(tmp)?;
            }
            std::fs::create_dir_all(tmp)?;
        }
        Ok(())
    }

    /// Cleans up all caches based on configured cache retention,
    /// in random order.
    ///
    /// If `dry_run` is `true`, no files will actually be deleted.
    pub fn cleanup(&self, dry_run: bool) -> Result<()> {
        // Destructure so we do not accidentally forget to cleanup one of our members.
        let Self {
            objects,
            object_meta,
            auxdifs,
            il2cpp,
            symcaches,
            cficaches,
            ppdb_caches,
            sourcemap_caches,
            sourcefiles,
            diagnostics,
            proguard,
        } = &self;

        // We want to clean up the caches in a random order. Ideally, this should not matter at all,
        // but we have seen some cleanup jobs getting stuck or dying reproducibly on certain types
        // of caches. A random shuffle increases the chance that the cleanup will make progress on
        // other caches.
        // The cleanup job dying on specific caches is a very bad thing and should definitely be
        // fixed, but in the meantime a random shuffle should provide more head room for a proper
        // fix in this case.
        let mut caches = vec![
            objects,
            object_meta,
            auxdifs,
            il2cpp,
            symcaches,
            cficaches,
            ppdb_caches,
            sourcemap_caches,
            sourcefiles,
            diagnostics,
            proguard,
        ];
        let mut rng = thread_rng();
        caches.as_mut_slice().shuffle(&mut rng);

        // Collect results so we can fail the entire function.  But we do not want to early
        // return since we should at least attempt to clean up all caches.
        let results: Vec<_> = caches.into_par_iter().map(|c| c.cleanup(dry_run)).collect();

        let mut first_error = None;
        for result in results {
            if let Err(err) = result {
                let stderr: &dyn std::error::Error = &*err;
                tracing::error!(stderr, "Failed to cleanup cache");
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[derive(Default)]
struct CleanupStats {
    removed_dirs: usize,
    removed_files: usize,
    removed_bytes: u64,
    removed_metadata_bytes: u64,

    retained_dirs: usize,
    retained_files: usize,
    retained_bytes: u64,
    retained_metadata_bytes: u64,
}

impl std::ops::AddAssign<Self> for CleanupStats {
    fn add_assign(&mut self, rhs: Self) {
        self.removed_dirs += rhs.removed_dirs;
        self.removed_files += rhs.removed_files;
        self.removed_bytes += rhs.removed_bytes;
        self.removed_metadata_bytes += rhs.removed_metadata_bytes;
        self.retained_dirs += rhs.retained_dirs;
        self.retained_files += rhs.retained_files;
        self.retained_bytes += rhs.retained_bytes;
        self.retained_metadata_bytes += rhs.retained_metadata_bytes;
    }
}

impl Cache {
    /// Cleans up this cache based on configured cache retention.
    ///
    /// If `dry_run` is `true`, no files will actually be deleted.
    pub fn cleanup(&self, dry_run: bool) -> Result<()> {
        tracing::info!("Cleaning up `{}` cache", self.name);
        let cache_dir = self.cache_dir.as_ref().ok_or_else(|| {
            anyhow!("no caching configured! Did you provide a path to your config file?")
        })?;

        let (_, stats) = self.cleanup_directory_recursive(cache_dir, dry_run)?;

        tracing::info!(
            retained.dirs = stats.retained_dirs,
            retained.files = stats.retained_files,
            retained.bytes = stats.retained_bytes,
            retained.metadata_bytes = stats.retained_metadata_bytes,
            removed.dirs = stats.removed_dirs,
            removed.files = stats.removed_files,
            removed.bytes = stats.removed_bytes,
            removed.metadata_bytes = stats.removed_metadata_bytes,
            "Cleaning up `{}` complete",
            self.name
        );

        metric!(gauge("caches.size.files") = stats.retained_files as u64, "cache" => self.name.as_ref());
        metric!(gauge("caches.size.bytes") = stats.retained_bytes, "cache" => self.name.as_ref());
        metric!(gauge("caches.size.metadata_bytes") = stats.retained_metadata_bytes, "cache" => self.name.as_ref());
        metric!(counter("caches.size.files_removed") += stats.removed_files as i64, "cache" => self.name.as_ref());
        metric!(counter("caches.size.bytes_removed") += stats.removed_bytes as i64, "cache" => self.name.as_ref());
        metric!(counter("caches.size.metadata_bytes_removed") += stats.removed_metadata_bytes as i64, "cache" => self.name.as_ref());

        Ok(())
    }

    /// Cleans up the directory recursively.
    ///
    /// Returns a boolean indicating whether the directory is left empty after cleanup,
    /// as well as statistics about how many directories/files/bytes were removed/retained.
    ///
    /// If `dry_run` is `true`, no files will actually be deleted.
    fn cleanup_directory_recursive(
        &self,
        directory: &Path,
        dry_run: bool,
    ) -> Result<(bool, CleanupStats)> {
        let entries = match catch_not_found(|| read_dir(directory))? {
            Some(x) => x,
            None => {
                tracing::warn!("Directory not found: `{}`", directory.display());
                return Ok((true, CleanupStats::default()));
            }
        };
        tracing::debug!("Cleaning directory `{}`", directory.display());

        let identity = || (true, CleanupStats::default());
        let (is_empty, stats) = entries
            .par_bridge()
            .try_fold(
                identity,
                |(mut is_empty, mut stats), entry| -> Result<(bool, CleanupStats)> {
                    let path = entry?.path();
                    // Skip metadata files—they will be handled together with their cache files.
                    if path.extension().and_then(OsStr::to_str) == Some(METADATA_EXTENSION) {
                        return Ok((is_empty, stats));
                    }
                    if path.is_dir() {
                        let (mut dir_is_empty, dir_stats) =
                            self.cleanup_directory_recursive(&path, dry_run)?;
                        stats += dir_stats;
                        if dir_is_empty {
                            tracing::debug!("Removing directory `{}`", directory.display());
                            if !dry_run {
                                if let Err(e) = remove_dir_all(&path) {
                                    sentry::with_scope(
                                        |scope| {
                                            scope.set_extra(
                                                "path",
                                                path.display().to_string().into(),
                                            )
                                        },
                                        || {
                                            tracing::error!(
                                                "Failed to clean cache directory: {:?}",
                                                e
                                            )
                                        },
                                    );
                                    dir_is_empty = false;
                                }
                            }
                        }
                        if dir_is_empty {
                            stats.removed_dirs += 1;
                        } else {
                            stats.retained_dirs += 1;
                        }
                        is_empty &= dir_is_empty;
                    } else {
                        match self.try_cleanup_path(&path, dry_run) {
                            Err(e) => {
                                sentry::with_scope(
                                    |scope| {
                                        scope.set_extra("path", path.display().to_string().into())
                                    },
                                    || tracing::error!("Failed to clean cache file: {:?}", e),
                                );
                            }
                            Ok((file_removed, file_stats)) => {
                                is_empty &= file_removed;
                                stats += file_stats;
                            }
                        }
                    }
                    Ok((is_empty, stats))
                },
            )
            .try_reduce(
                identity,
                |(is_empty_1, mut stats1), (is_empty_2, stats2)| {
                    stats1 += stats2;

                    Ok((is_empty_1 & is_empty_2, stats1))
                },
            )?;

        Ok((is_empty, stats))
    }

    /// Tries to clean up the file at `path`.
    ///
    /// Returns a boolean indicating whether the file was removed,
    /// as well as statistics about how many files/bytes were removed/retained.
    ///
    /// This also removes the file's corresponding metadata file, if it exists.
    /// If `dry_run` is `true`, the file will not actually be deleted.
    fn try_cleanup_path(&self, path: &Path, dry_run: bool) -> Result<(bool, CleanupStats)> {
        tracing::trace!("Checking file `{}`", path.display());
        let mut stats = CleanupStats::default();
        let Some(metadata) = catch_not_found(|| path.metadata())? else {
            return Ok((true, stats));
        };
        anyhow::ensure!(metadata.is_file(), "not a file");
        let size = metadata.len();

        let metadata_path = metadata_path(path);
        let metadata_size =
            catch_not_found(|| metadata_path.metadata())?.map_or(0, |metadata| metadata.len());

        if catch_not_found(|| self.check_expiry(path))?.is_none() {
            tracing::debug!("Removing file `{}`", path.display());
            if !dry_run {
                catch_not_found(|| remove_file(path))?;
                catch_not_found(|| remove_file(&metadata_path))?;
            }

            stats.removed_bytes += size;
            stats.removed_metadata_bytes += metadata_size;
            stats.removed_files += 1;

            Ok((true, stats))
        } else {
            stats.retained_bytes += size;
            stats.retained_metadata_bytes += metadata_size;
            stats.retained_files += 1;

            Ok((false, stats))
        }
    }
}
