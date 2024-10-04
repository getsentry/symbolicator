use std::fs::{read_dir, remove_dir, remove_file};
use std::io;
use std::path::Path;

use anyhow::{anyhow, Result};
use rand::seq::SliceRandom;
use rand::thread_rng;

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
        let results: Vec<_> = caches.into_iter().map(|c| c.cleanup(dry_run)).collect();

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

    retained_dirs: usize,
    retained_files: usize,
    retained_bytes: u64,
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

        let mut stats = CleanupStats::default();
        self.cleanup_directory_recursive(cache_dir, &mut stats, dry_run)?;

        tracing::info!("Cleaning up `{}` complete", self.name);
        tracing::info!(
            "Retained {} directories and {} files, totaling {} bytes",
            stats.retained_dirs,
            stats.retained_files,
            stats.retained_bytes,
        );
        tracing::info!(
            "Removed {} directories and {} files, totaling {} bytes",
            stats.removed_dirs,
            stats.removed_files,
            stats.removed_bytes
        );

        metric!(gauge("caches.size.files") = stats.retained_files as u64, "cache" => self.name.as_ref());
        metric!(gauge("caches.size.bytes") = stats.retained_bytes, "cache" => self.name.as_ref());
        metric!(counter("caches.size.files_removed") += stats.removed_files as i64, "cache" => self.name.as_ref());
        metric!(counter("caches.size.bytes_removed") += stats.removed_bytes as i64, "cache" => self.name.as_ref());

        Ok(())
    }

    /// Cleans up the directory recursively, returning `true` if the directory is left empty after cleanup.
    ///
    /// If `dry_run` is `true`, no files will actually be deleted.
    fn cleanup_directory_recursive(
        &self,
        directory: &Path,
        stats: &mut CleanupStats,
        dry_run: bool,
    ) -> Result<bool> {
        let entries = match catch_not_found(|| read_dir(directory))? {
            Some(x) => x,
            None => {
                tracing::warn!("Directory not found: `{}`", directory.display());
                return Ok(true);
            }
        };
        tracing::debug!("Cleaning directory `{}`", directory.display());

        let mut is_empty = true;
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                let mut dir_is_empty = self.cleanup_directory_recursive(&path, stats, dry_run)?;
                if dir_is_empty {
                    tracing::debug!("Removing directory `{}`", directory.display());
                    if !dry_run {
                        if let Err(e) = remove_dir(&path) {
                            sentry::with_scope(
                                |scope| scope.set_extra("path", path.display().to_string().into()),
                                || tracing::error!("Failed to clean cache directory: {:?}", e),
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
                match self.try_cleanup_path(&path, stats, dry_run) {
                    Err(e) => {
                        sentry::with_scope(
                            |scope| scope.set_extra("path", path.display().to_string().into()),
                            || tracing::error!("Failed to clean cache file: {:?}", e),
                        );
                    }
                    Ok(file_removed) => is_empty &= file_removed,
                }
            }
        }

        Ok(is_empty)
    }

    /// Tries to clean up the file at `path`, returning `true` if it was removed.
    ///
    /// If `dry_run` is `true`, the file will not actually be deleted.
    fn try_cleanup_path(
        &self,
        path: &Path,
        stats: &mut CleanupStats,
        dry_run: bool,
    ) -> Result<bool> {
        tracing::trace!("Checking file `{}`", path.display());
        let Some(metadata) = catch_not_found(|| path.metadata())? else {
            return Ok(true);
        };
        anyhow::ensure!(metadata.is_file(), "not a file");
        let size = metadata.len();

        if catch_not_found(|| self.check_expiry(path))?.is_none() {
            tracing::debug!("Removing file `{}`", path.display());
            if !dry_run {
                catch_not_found(|| remove_file(path))?;
            }

            stats.removed_bytes += size;
            stats.removed_files += 1;

            return Ok(true);
        }
        stats.retained_bytes += size;
        stats.retained_files += 1;

        Ok(false)
    }
}
