/// Core logic for cache files. Used by `crate::actors::cache`.
///
/// Terminology:
/// * positive cache item: A cache item that represents the presence of something. E.g. we succeeded in downloading an object file and cached that file.
/// * Negative cache item: A cache item that represents the absence of something. E.g. we encountered a 404 while trying to download a file, and cached that fact. Represented by an empty file.
///
/// * item age: When the item was computed and created.
/// * item last used: Last time this item was involved in a cache hit.
///
/// TODO:
/// * We want to try upgrading derived caches without pruning them. This will likely require the concept of a content checksum (which would just be the cache key of the object file that would be used to create the derived cache.
use std::fs::{create_dir_all, read_dir, remove_file, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use failure::Fail;

use sentry::integrations::failure::capture_fail;
use symbolic::common::ByteView;

use crate::config::CacheConfig;
use crate::logging::LogError;
use crate::types::Scope;

/// Content of symcaches/cficaches whose writing failed.
///
/// Items with this value will be considered expired after the next process restart, or will be
/// pruned once `symbolicator cleanup` runs. Independently of any `max_age` or `max_last_used`.
///
/// The malformed state is useful for failed computations that are unlikely to succeed before the
/// next deploy. For example, symcache writing may fail due to an object file symbolic can't parse
/// yet.
pub const MALFORMED_MARKER: &[u8] = b"malformed";

#[derive(Debug, Fail, derive_more::From)]
pub enum CleanupError {
    #[fail(display = "No caching configured! Did you provide a path to your config file?")]
    NoCachingConfigured,

    #[fail(display = "Not a file")]
    NotAFile,

    #[fail(display = "Failed to access filesystem")]
    Io(#[fail(cause)] io::Error),
}

/// Utilities for a sym/cfi or object cache.
#[derive(Debug, Clone)]
pub struct Cache {
    /// Cache identifier used for metric names.
    pub name: &'static str,

    /// Directory to use for storing cache items. Will be created if it does not exist.
    pub cache_dir: Option<PathBuf>,

    /// Time when this process started.
    start_time: SystemTime,

    /// Options intended to be user-configurable.
    pub cache_config: CacheConfig,
}

impl Cache {
    pub fn new<P: AsRef<Path>>(
        name: &'static str,
        cache_dir: Option<P>,
        cache_config: CacheConfig,
    ) -> Self {
        Cache {
            name,
            cache_dir: cache_dir.map(|x| x.as_ref().to_owned()),
            start_time: SystemTime::now(),
            cache_config,
        }
    }

    pub fn cleanup(&self) -> Result<(), CleanupError> {
        log::info!("Cleaning up cache: {}", self.name);
        let cache_dir = match self.cache_dir {
            Some(ref x) => x,
            None => return Err(CleanupError::NoCachingConfigured),
        };

        let entries = match catch_not_found(|| read_dir(cache_dir))? {
            Some(x) => x,
            None => {
                log::warn!("Directory not found");
                return Ok(());
            }
        };

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if let Err(e) = self.try_cleanup_path(&path) {
                log::error!("Failed to clean up {}: {}", path.display(), LogError(&e));
                capture_fail(&e);
            }
        }

        Ok(())
    }

    fn try_cleanup_path(&self, path: &Path) -> Result<(), CleanupError> {
        log::debug!("Looking at {}", path.display());
        if path.is_file() {
            if catch_not_found(|| self.check_expiry(path))?.is_none() {
                log::info!("Removing {}", path.display());
                catch_not_found(|| remove_file(path))?;
            }

            Ok(())
        } else {
            Err(CleanupError::NotAFile)
        }
    }

    /// Validate cache expiration of path. If cache should not be used,
    /// `Err(io::ErrorKind::NotFound)` is returned.  If cache is usable, `Ok(x)` is returned, where
    /// `x` indicates whether the file should be touched before using.
    fn check_expiry(&self, path: &Path) -> io::Result<bool> {
        let metadata = path.metadata()?;

        log::trace!("File length: {}", metadata.len());

        // Immediately expire malformed items that have been created before this process started.
        // See docstring of MALFORMED_MARKER
        if MALFORMED_MARKER.len() as u64 == metadata.len() {
            let created_at = metadata.created()?;
            if created_at < self.start_time {
                log::trace!("Created at is older than start time");
                let mut file = File::open(path)?;
                let mut buf = vec![0; MALFORMED_MARKER.len()];
                file.read_exact(&mut buf)?;

                log::trace!("First {} bytes: {:?}", buf.len(), buf);

                if buf == MALFORMED_MARKER {
                    return Err(io::ErrorKind::NotFound.into());
                }
            }
        }

        let is_negative = metadata.len() == 0;

        // Translate externally visible caching config to internal concepts of "access time" and
        // "creation time"
        let (max_age, max_last_used) = if is_negative {
            (None, self.cache_config.max_unused_for)
        } else {
            (self.cache_config.retry_misses_after, None)
        };

        if let Some(max_age) = max_age {
            let created_at = metadata.created()?;

            if created_at.elapsed().map(|x| x > max_age).unwrap_or(true) {
                return Err(io::ErrorKind::NotFound.into());
            }
        }

        // We use mtime to keep track of "cache last used", because filesystem is usually mounted
        // with `noatime` and therefore atime is nonsense.
        let last_used = if let Some(max_last_used) = max_last_used {
            let last_used = metadata.modified()?.elapsed().ok();

            if last_used.map(|x| x > max_last_used).unwrap_or(true) {
                return Err(io::ErrorKind::NotFound.into());
            }

            last_used
        } else {
            None
        };

        Ok(last_used
            .map(|x| x > Duration::from_secs(3600))
            .unwrap_or(true))
    }

    /// Validate cachefile against expiration config and open a byteview on it. Takes care of
    /// bumping mtime.
    pub fn open_cachefile(&self, path: &Path) -> io::Result<Option<ByteView<'static>>> {
        // `io::ErrorKind::NotFound` can be returned from multiple locations in this function. All
        // of those can indicate a cache miss as cache cleanup can run inbetween. Only when we have
        // an open ByteView we can be sure to have a cache hit.
        catch_not_found(|| {
            let should_touch = self.check_expiry(path)?;

            if should_touch {
                OpenOptions::new()
                    .append(true)
                    .truncate(false)
                    .open(&path)?;
            }

            ByteView::open(path)
        })
    }
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct CacheKey {
    pub cache_key: String,
    pub scope: Scope,
}

pub fn get_scope_path(
    cache_dir: Option<&Path>,
    scope: &Scope,
    cache_key: &str,
) -> Result<Option<PathBuf>, io::Error> {
    let dir = match cache_dir {
        Some(x) => x.join(safe_path_segment(scope.as_ref())),
        None => return Ok(None),
    };

    create_dir_all(&dir)?;
    Ok(Some(dir.join(safe_path_segment(cache_key))))
}

fn safe_path_segment(s: &str) -> String {
    s.replace(".", "_") // protect against ..
        .replace("/", "_") // protect against absolute paths
        .replace(":", "_") // not a threat on POSIX filesystems, but confuses OS X Finder
}

fn catch_not_found<F, R>(f: F) -> io::Result<Option<R>>
where
    F: FnOnce() -> io::Result<R>,
{
    match f() {
        Ok(x) => Ok(Some(x)),
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound => Ok(None),
            _ => Err(e),
        },
    }
}
