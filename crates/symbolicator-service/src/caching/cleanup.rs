use std::fs::{read_dir, remove_dir, remove_file};
use std::io;
use std::path::Path;

use anyhow::{anyhow, Result};

use crate::config::Config;

use super::fs::catch_not_found;
use super::{Cache, Caches};

/// Entry function for the cleanup command.
///
/// This will clean up all caches based on configured cache retention.
pub fn cleanup(config: Config) -> Result<()> {
    Caches::from_config(&config)?.cleanup()
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

    pub fn cleanup(&self) -> Result<()> {
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
            diagnostics,
        } = &self;

        // Collect results so we can fail the entire function.  But we do not want to early
        // return since we should at least attempt to clean up all caches.
        let results = vec![
            objects.cleanup(),
            object_meta.cleanup(),
            symcaches.cleanup(),
            cficaches.cleanup(),
            diagnostics.cleanup(),
            auxdifs.cleanup(),
            il2cpp.cleanup(),
            ppdb_caches.cleanup(),
            sourcemap_caches.cleanup(),
        ];

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

impl Cache {
    pub fn cleanup(&self) -> Result<()> {
        tracing::info!("Cleaning up cache: {}", self.name);
        let cache_dir = self.cache_dir.as_ref().ok_or_else(|| {
            anyhow!("no caching configured! Did you provide a path to your config file?")
        })?;

        self.cleanup_directory_recursive(cache_dir)?;

        Ok(())
    }

    /// Cleans up the directory recursively, returning `true` if the directory is left empty after cleanup.
    fn cleanup_directory_recursive(&self, directory: &Path) -> Result<bool> {
        let entries = match catch_not_found(|| read_dir(directory))? {
            Some(x) => x,
            None => {
                tracing::warn!("Directory not found: {}", directory.display());
                return Ok(true);
            }
        };

        let mut is_empty = true;
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                let mut dir_is_empty = self.cleanup_directory_recursive(&path)?;
                if dir_is_empty {
                    if let Err(e) = remove_dir(&path) {
                        sentry::with_scope(
                            |scope| scope.set_extra("path", path.display().to_string().into()),
                            || tracing::error!("Failed to clean cache directory: {:?}", e),
                        );
                        dir_is_empty = false;
                    }
                }
                is_empty &= dir_is_empty;
            } else {
                match self.try_cleanup_path(&path) {
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
    fn try_cleanup_path(&self, path: &Path) -> Result<bool> {
        tracing::trace!("Checking {}", path.display());
        anyhow::ensure!(path.is_file(), "not a file");
        if catch_not_found(|| self.check_expiry(path))?.is_none() {
            tracing::debug!("Removing {}", path.display());
            catch_not_found(|| remove_file(path))?;

            return Ok(true);
        }

        Ok(false)
    }
}
