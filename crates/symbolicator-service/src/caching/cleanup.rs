use std::ffi::OsStr;
use std::fs::{read_dir, remove_dir_all, remove_file};
use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow};
use rand::seq::SliceRandom;
use rayon::prelude::*;

use crate::caching::fs::{METADATA_EXTENSION, metadata_path};
use crate::config::Config;

use super::fs::catch_not_found;
use super::{Cache, Caches};

/// Entry function for the cleanup command.
///
/// This will clean up all caches based on configured cache retention.
/// If `dry_run` is `true`, no files will actually be deleted.
///
/// There are three possible cases for `repeat`:
/// * `None`: Don't loop.
/// * `Some(None)`: Loop with the interval controlled by the `cache_cleanup_interval` config option.
/// * `Some(Some(interval))`: Loop with the given interval.
pub fn cleanup(config: Config, dry_run: bool, repeat: Option<Option<Duration>>) -> Result<()> {
    let loop_interval = repeat.map(|interval| interval.unwrap_or(config.cache_cleanup_interval));
    Caches::from_config(&config)?.cleanup(dry_run, loop_interval)
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
    ///
    /// If `loop_interval` is `Some(interval)`, this function will
    /// loop with a sleep of length `interval` between iterations.
    pub fn cleanup(&self, dry_run: bool, loop_interval: Option<Duration>) -> Result<()> {
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
            source_index,
        } = &self;

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
            source_index,
        ];
        let mut rng = rand::rng();

        // If `loop_interval` is `None` we break out of this loop after the first iteration.
        loop {
            // We want to clean up the caches in a random order. Ideally, this should not matter at all,
            // but we have seen some cleanup jobs getting stuck or dying reproducibly on certain types
            // of caches. A random shuffle increases the chance that the cleanup will make progress on
            // other caches.
            // The cleanup job dying on specific caches is a very bad thing and should definitely be
            // fixed, but in the meantime a random shuffle should provide more head room for a proper
            // fix in this case.
            caches.as_mut_slice().shuffle(&mut rng);

            // Collect results so we can fail the entire function.  But we do not want to early
            // return since we should at least attempt to clean up all caches.
            let results: Vec<_> = caches.par_iter().map(|c| c.cleanup(dry_run)).collect();

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

            match loop_interval {
                Some(interval) => std::thread::sleep(interval),
                None => {
                    break match first_error {
                        Some(err) => Err(err),
                        None => Ok(()),
                    };
                }
            }
        }
    }
}

impl Cache {
    /// Cleans up this cache based on configured cache retention.
    ///
    /// If `dry_run` is `true`, no files will actually be deleted.
    #[tracing::instrument(skip(self), fields(cache = %self.name))]
    pub fn cleanup(&self, dry_run: bool) -> Result<()> {
        let cache_dir = self.cache_dir.as_ref().ok_or_else(|| {
            anyhow!("no caching configured! Did you provide a path to your config file?")
        })?;

        tracing::info!("Cleaning up `{}` cache", self.name);

        metric!(gauge("caches.size.bytes") = 0.0, "cache" => self.name.as_str());
        metric!(gauge("caches.size.metadata_bytes") = 0.0, "cache" => self.name.as_str());
        metric!(gauge("caches.size.files") = 0.0, "cache" => self.name.as_str());

        self.cleanup_directory_recursive(cache_dir, dry_run)?;

        Ok(())
    }

    /// Cleans up the directory recursively.
    ///
    /// Returns a boolean indicating whether the directory is left empty after cleanup.
    ///
    /// If `dry_run` is `true`, no files will actually be deleted.
    fn cleanup_directory_recursive(&self, directory: &Path, dry_run: bool) -> Result<bool> {
        let entries = match catch_not_found(|| read_dir(directory))? {
            Some(x) => x,
            None => {
                tracing::warn!("Directory not found: `{}`", directory.display());
                return Ok(true);
            }
        };
        tracing::debug!("Cleaning directory `{}`", directory.display());

        let is_empty = entries
            .par_bridge()
            .try_fold(
                || true,
                |mut is_empty, entry| -> Result<bool> {
                    let path = entry?.path();
                    // Skip metadata files—they will be handled together with their cache files.
                    if path.extension().and_then(OsStr::to_str) == Some(METADATA_EXTENSION) {
                        return Ok(is_empty);
                    }
                    if path.is_dir() {
                        let mut dir_is_empty = self.cleanup_directory_recursive(&path, dry_run)?;
                        if dir_is_empty {
                            tracing::debug!("Removing directory `{}`", directory.display());
                            if !dry_run && let Err(e) = remove_dir_all(&path) {
                                sentry::with_scope(
                                    |scope| {
                                        scope.set_extra("path", path.display().to_string().into())
                                    },
                                    || tracing::error!("Failed to clean cache directory: {:?}", e),
                                );
                                dir_is_empty = false;
                            }
                        }
                        is_empty &= dir_is_empty;
                    } else {
                        match self.try_cleanup_path(&path, dry_run) {
                            Err(e) => {
                                sentry::with_scope(
                                    |scope| {
                                        scope.set_extra("path", path.display().to_string().into())
                                    },
                                    || tracing::error!("Failed to clean cache file: {e:?}"),
                                );
                            }
                            Ok(file_removed) => {
                                is_empty &= file_removed;
                            }
                        }
                    }
                    Ok(is_empty)
                },
            )
            .try_reduce(
                || true,
                |is_empty_1, is_empty_2| Ok(is_empty_1 & is_empty_2),
            )?;

        Ok(is_empty)
    }

    /// Tries to clean up the file at `path`.
    ///
    /// Returns a boolean indicating whether the file was removed.
    ///
    /// This also removes the file's corresponding metadata file, if it exists.
    /// If `dry_run` is `true`, the file will not actually be deleted.
    fn try_cleanup_path(&self, path: &Path, dry_run: bool) -> Result<bool> {
        tracing::trace!("Checking file `{}`", path.display());
        let Some(metadata) = catch_not_found(|| path.metadata())? else {
            return Ok(true);
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

            metric!(counter("caches.size.bytes_removed") += size, "cache" => self.name.as_str());
            metric!(counter("caches.size.metadata_bytes_removed") += metadata_size, "cache" => self.name.as_str());
            metric!(counter("caches.size.files_removed") += 1, "cache" => self.name.as_str());

            Ok(true)
        } else {
            metric!(gauge("caches.size.bytes") += size as f64, "cache" => self.name.as_str());
            metric!(gauge("caches.size.metadata_bytes") += metadata_size as f64, "cache" => self.name.as_str());
            metric!(gauge("caches.size.files") += 1, "cache" => self.name.as_str());

            Ok(false)
        }
    }
}
