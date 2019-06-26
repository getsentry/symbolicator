/// Core logic for cache files. Used by `crate::actors::common::cache`.
///
/// TODO:
/// * We want to try upgrading derived caches without pruning them. This will likely require the concept of a content checksum (which would just be the cache key of the object file that would be used to create the derived cache.
use std::fs::{read_dir, remove_file, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use failure::Fail;
use sentry::integrations::failure::capture_fail;
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::config::CacheConfig;
use crate::logging::LogError;
use crate::types::Scope;

/// Content of cache items whose writing failed.
///
/// Items with this value will be considered expired after the next process restart, or will be
/// pruned once `symbolicator cleanup` runs. Independently of any `max_age` or `max_last_used`.
///
/// The malformed state is useful for failed computations that are unlikely to succeed before the
/// next deploy. For example, symcache writing may fail due to an object file symbolic can't parse
/// yet.
const MALFORMED_MARKER: &[u8] = b"malformed";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CacheStatus {
    /// A cache item that represents the presence of something. E.g. we succeeded in downloading an
    /// object file and cached that file.
    Positive,
    /// A cache item that represents the absence of something. E.g. we encountered a 404 while
    /// trying to download a file, and cached that fact. Represented by an empty file.
    Negative,
    /// We are unable to create or use the cache item. E.g. we failed to create a symcache. See
    /// docs for `MALFORMED_MARKER`.
    Malformed,
}

impl AsRef<str> for CacheStatus {
    fn as_ref(&self) -> &str {
        match self {
            CacheStatus::Positive => "positive",
            CacheStatus::Negative => "negative",
            CacheStatus::Malformed => "malformed",
        }
    }
}

impl CacheStatus {
    pub fn from_content(s: &[u8]) -> CacheStatus {
        if s == MALFORMED_MARKER {
            CacheStatus::Malformed
        } else if s.is_empty() {
            CacheStatus::Negative
        } else {
            CacheStatus::Positive
        }
    }

    pub fn persist_item(self, path: &Path, file: NamedTempFile) -> Result<(), io::Error> {
        match self {
            CacheStatus::Positive => {
                file.persist(path).map_err(|x| x.error)?;
            }
            CacheStatus::Negative => {
                File::create(path)?;
            }
            CacheStatus::Malformed => {
                let mut f = File::create(path)?;
                f.write_all(MALFORMED_MARKER)?;
            }
        }

        Ok(())
    }
}

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
    name: &'static str,

    /// Directory to use for storing cache items. Will be created if it does not exist.
    cache_dir: Option<PathBuf>,

    /// Time when this process started.
    start_time: SystemTime,

    /// Options intended to be user-configurable.
    cache_config: CacheConfig,
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

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_ref().map(|x| &**x)
    }

    pub fn cleanup(&self) -> Result<(), CleanupError> {
        log::info!("Cleaning up cache: {}", self.name);
        let cache_dir = match self.cache_dir {
            Some(ref x) => x.clone(),
            None => return Err(CleanupError::NoCachingConfigured),
        };

        let mut directories = vec![cache_dir];
        while !directories.is_empty() {
            let directory = directories.pop().unwrap();

            let entries = match catch_not_found(|| read_dir(directory))? {
                Some(x) => x,
                None => {
                    log::warn!("Directory not found");
                    return Ok(());
                }
            };

            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    directories.push(path.to_owned());
                } else if let Err(e) = self.try_cleanup_path(&path) {
                    log::error!("Failed to clean up {}: {}", path.display(), LogError(&e));
                    capture_fail(&e);
                }
            }
        }

        Ok(())
    }

    fn try_cleanup_path(&self, path: &Path) -> Result<(), CleanupError> {
        log::trace!("Checking {}", path.display());
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
        // We use mtime to keep track of both "cache last used" and "cache created" depending on
        // whether the file is a negative cache item or not, because literally every other
        // filesystem attribute is unreliable.
        //
        // * creation time does not exist pre-Linux 4.11
        // * most filesystems are mounted with noatime
        //
        // States a cache item can be in:
        // * negative/empty: An empty file. Represents a failed download. mtime is used to indicate
        //   when the failed download happened (when the file was created)
        // * malformed: A file with the content `b"malformed"`. Represents a failed symcache
        //   conversion. mtime indicates when we attempted to convert.
        // * ok (don't really have a name): File has any other content, mtime is used to keep track
        //   of last use.
        let metadata = path.metadata()?;

        log::trace!("File length: {}", metadata.len());

        let is_malformed = if MALFORMED_MARKER.len() as u64 == metadata.len() {
            let mut file = File::open(path)?;
            let mut buf = vec![0; MALFORMED_MARKER.len()];
            file.read_exact(&mut buf)?;

            log::trace!("First {} bytes: {:?}", buf.len(), buf);
            buf == MALFORMED_MARKER
        } else {
            false
        };

        let is_negative = metadata.len() == 0;

        if is_malformed {
            // Immediately expire malformed items that have been created before this process started.
            // See docstring of MALFORMED_MARKER

            let created_at = metadata.modified()?;

            let retry_malformed = if let (Ok(elapsed), Some(retry_malformed_after)) = (
                created_at.elapsed(),
                self.cache_config.retry_malformed_after,
            ) {
                elapsed > retry_malformed_after
            } else {
                false
            };

            if created_at < self.start_time || retry_malformed {
                log::trace!("Created at is older than start time");
                return Err(io::ErrorKind::NotFound.into());
            }
        }

        let max_mtime = if is_negative {
            self.cache_config.retry_misses_after
        } else {
            self.cache_config.max_unused_for
        };

        let mtime = if let Some(max_mtime) = max_mtime {
            let mtime = metadata.modified()?.elapsed().ok();

            if mtime.map(|x| x > max_mtime).unwrap_or(true) {
                return Err(io::ErrorKind::NotFound.into());
            }

            mtime
        } else {
            None
        };

        Ok(!is_negative
            && !is_malformed
            && mtime.map(|x| x > Duration::from_secs(1200)).unwrap_or(true))
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

pub fn get_scope_path(cache_dir: Option<&Path>, scope: &Scope, cache_key: &str) -> Option<PathBuf> {
    Some(
        cache_dir?
            .join(safe_path_segment(scope.as_ref()))
            .join(safe_path_segment(cache_key)),
    )
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

#[cfg(test)]
fn tempdir() -> io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(".")
}

#[test]
fn test_max_unused_for() -> Result<(), CleanupError> {
    use std::fs::create_dir_all;
    use std::io::Write;
    use std::thread::sleep;

    let tempdir = tempdir()?;
    create_dir_all(tempdir.path().join("foo"))?;

    let cache = Cache::new(
        "test",
        Some(tempdir.path()),
        CacheConfig {
            max_unused_for: Some(Duration::from_millis(10)),
            ..CacheConfig::default_derived()
        },
    );

    File::create(tempdir.path().join("foo/killthis"))?.write_all(b"hi")?;
    File::create(tempdir.path().join("foo/keepthis"))?.write_all(b"")?;
    sleep(Duration::from_millis(11));

    File::create(tempdir.path().join("foo/keepthis2"))?.write_all(b"hi")?;
    cache.cleanup()?;

    let mut basenames: Vec<_> = read_dir(tempdir.path().join("foo"))?
        .map(|x| x.unwrap().file_name().into_string().unwrap())
        .collect();

    basenames.sort();

    assert_eq!(basenames, vec!["keepthis", "keepthis2"]);

    Ok(())
}

#[test]
fn test_retry_misses_after() -> Result<(), CleanupError> {
    use std::fs::create_dir_all;
    use std::io::Write;
    use std::thread::sleep;

    let tempdir = tempdir()?;
    create_dir_all(tempdir.path().join("foo"))?;

    let cache = Cache::new(
        "test",
        Some(tempdir.path()),
        CacheConfig {
            retry_misses_after: Some(Duration::from_millis(20)),
            ..CacheConfig::default_derived()
        },
    );

    File::create(tempdir.path().join("foo/keepthis"))?.write_all(b"hi")?;
    File::create(tempdir.path().join("foo/killthis"))?.write_all(b"")?;
    sleep(Duration::from_millis(25));

    File::create(tempdir.path().join("foo/keepthis2"))?.write_all(b"")?;
    cache.cleanup()?;

    let mut basenames: Vec<_> = read_dir(tempdir.path().join("foo"))?
        .map(|x| x.unwrap().file_name().into_string().unwrap())
        .collect();

    basenames.sort();

    assert_eq!(basenames, vec!["keepthis", "keepthis2"]);

    Ok(())
}

#[test]
fn test_cleanup_malformed() -> Result<(), CleanupError> {
    use std::fs::create_dir_all;
    use std::io::Write;
    use std::thread::sleep;

    let tempdir = tempdir()?;
    create_dir_all(tempdir.path().join("foo"))?;

    // File has same amount of chars as "malformed", check that optimization works
    File::create(tempdir.path().join("foo/keepthis"))?.write_all(b"addictive")?;
    File::create(tempdir.path().join("foo/keepthis2"))?.write_all(b"hi")?;

    File::create(tempdir.path().join("foo/killthis"))?.write_all(b"malformed")?;

    sleep(Duration::from_millis(10));

    // Creation of this struct == "process startup"
    let cache = Cache::new("test", Some(tempdir.path()), CacheConfig::default_derived());

    cache.cleanup()?;

    let mut basenames: Vec<_> = read_dir(tempdir.path().join("foo"))?
        .map(|x| x.unwrap().file_name().into_string().unwrap())
        .collect();

    basenames.sort();

    assert_eq!(basenames, vec!["keepthis", "keepthis2"]);

    Ok(())
}
