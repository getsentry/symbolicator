/// Core logic for cache files. Used by `crate::services::common::cache`.
///
/// TODO:
/// * We want to try upgrading derived caches without pruning them. This will likely require the concept of a content checksum (which would just be the cache key of the object file that would be used to create the derived cache.
use std::fs::{self, read_dir, remove_file, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::config::{CacheConfig, Config};
use crate::types::Scope;

/// Content of cache items whose writing failed.
///
/// Items with this value will be considered expired after the next process restart, or will be
/// pruned once `symbolicator cleanup` runs. Independently of any `max_age` or `max_last_used`.
///
/// The malformed state is useful for failed computations that are unlikely to succeed before the
/// next deploy. For example, symcache writing may fail due to an object file symbolic can't parse
/// yet.
pub const MALFORMED_MARKER: &[u8] = b"malformed";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CacheStatus {
    /// A cache item that represents the presence of something. E.g. we succeeded in downloading an
    /// object file and cached that file.
    Positive,
    /// A cache item that represents the absence of something. E.g. we encountered a 404 while
    /// trying to download a file, and cached that fact. Represented by an empty file.
    Negative,
    /// We are unable to create or use the cache item. E.g. we failed to create a symcache. See
    /// docs for [`MALFORMED_MARKER`].
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

    /// Persist the operation in the cache.
    ///
    /// If the status was [`CacheStatus::Positive`] this copies the data from the temporary
    /// file to the final cache location.  Otherwise it writes corresponding marker in the
    /// cache location.
    pub fn persist_item(self, path: &Path, file: NamedTempFile) -> Result<(), io::Error> {
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "no parent directory to persist item")
        })?;
        fs::create_dir_all(dir)?;
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

/// Utilities for a sym/cfi or object cache.
#[derive(Debug, Clone)]
pub struct Cache {
    /// Cache identifier used for metric names.
    name: &'static str,

    /// Directory to use for storing cache items. Will be created if it does not exist.
    ///
    /// Leaving this as None will disable this cache.
    cache_dir: Option<PathBuf>,

    /// Directory to use for temporary files.
    ///
    /// When writing a new file into the cache it is best to write it to a temporary file in
    /// a sibling directory, once fully written it can then be atomically moved to the
    /// actual location withing the [`cache_dir`](Self::cache_dir).
    ///
    /// Just like for `cache_dir` when this cache is disabled this will be `None`.
    tmp_dir: Option<PathBuf>,

    /// Time when this process started.
    start_time: SystemTime,

    /// Options intended to be user-configurable.
    cache_config: CacheConfig,
}

impl Cache {
    pub fn from_config(
        name: &'static str,
        cache_dir: Option<PathBuf>,
        tmp_dir: Option<PathBuf>,
        cache_config: CacheConfig,
    ) -> io::Result<Self> {
        if let Some(ref dir) = cache_dir {
            std::fs::create_dir_all(dir)?;
        }
        Ok(Cache {
            name,
            cache_dir,
            tmp_dir,
            start_time: SystemTime::now(),
            cache_config,
        })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_deref()
    }

    pub fn cleanup(&self) -> Result<()> {
        log::info!("Cleaning up cache: {}", self.name);
        let cache_dir = self.cache_dir.clone().ok_or_else(|| {
            anyhow!("no caching configured! Did you provide a path to your config file?")
        })?;

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
                    sentry::with_scope(
                        |scope| scope.set_extra("path", path.display().to_string().into()),
                        || log::error!("Failed to clean cache file: {:?}", e),
                    );
                }
            }
        }

        Ok(())
    }

    fn try_cleanup_path(&self, path: &Path) -> Result<()> {
        log::trace!("Checking {}", path.display());
        anyhow::ensure!(path.is_file(), "not a file");
        if catch_not_found(|| self.check_expiry(path))?.is_none() {
            log::debug!("Removing {}", path.display());
            catch_not_found(|| remove_file(path))?;
        }

        Ok(())
    }

    /// Validate cache expiration of path. If cache should not be used,
    /// `Err(io::ErrorKind::NotFound)` is returned. If cache is usable, `Ok(x)` is returned, where
    /// `x` indicates whether the file should be touched before using.
    fn check_expiry(&self, path: &Path) -> io::Result<bool> {
        // We use `mtime` to keep track of both "cache last used" and "cache created" depending on
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
                self.cache_config.retry_malformed_after(),
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
            self.cache_config.retry_misses_after()
        } else {
            self.cache_config.max_unused_for()
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
            && mtime.map(|x| x > Duration::from_secs(3600)).unwrap_or(true))
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

    /// Create a new temporary file to use in the cache.
    pub fn tempfile(&self) -> io::Result<NamedTempFile> {
        match self.tmp_dir {
            Some(ref path) => {
                std::fs::create_dir_all(path)?;
                Ok(tempfile::Builder::new().prefix("tmp").tempfile_in(path)?)
            }
            None => Ok(NamedTempFile::new()?),
        }
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
    s.replace(".", "_") // protect against ".."
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

pub struct Caches {
    pub objects: Cache,
    pub object_meta: Cache,
    pub symcaches: Cache,
    pub cficaches: Cache,
    pub diagnostics: Cache,
}

impl Caches {
    pub fn from_config(config: &Config) -> io::Result<Self> {
        let tmp_dir = config.cache_dir("tmp");
        Ok(Self {
            objects: {
                let path = config.cache_dir("objects");
                Cache::from_config(
                    "objects",
                    path,
                    tmp_dir.clone(),
                    config.caches.downloaded.into(),
                )?
            },
            object_meta: {
                let path = config.cache_dir("object_meta");
                Cache::from_config(
                    "object_meta",
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                )?
            },
            symcaches: {
                let path = config.cache_dir("symcaches");
                Cache::from_config(
                    "symcaches",
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                )?
            },
            cficaches: {
                let path = config.cache_dir("cficaches");
                Cache::from_config(
                    "cficaches",
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                )?
            },
            diagnostics: {
                let path = config.cache_dir("diagnostics");
                Cache::from_config(
                    "diagnostics",
                    path,
                    tmp_dir,
                    config.caches.diagnostics.into(),
                )?
            },
        })
    }

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
        self.objects.cleanup()?;
        self.object_meta.cleanup()?;
        self.symcaches.cleanup()?;
        self.cficaches.cleanup()?;
        Ok(())
    }
}

/// Entry function for the cleanup command.
///
/// This will clean up all caches based on configured cache retention.
pub fn cleanup(config: Config) -> Result<()> {
    Caches::from_config(&config)?.cleanup()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::{self, create_dir_all};
    use std::io::Write;
    use std::thread::sleep;

    use crate::config::DerivedCacheConfig;

    fn tempdir() -> io::Result<tempfile::TempDir> {
        tempfile::tempdir_in(".")
    }

    #[test]
    fn test_cache_dir_created() {
        let basedir = tempdir().unwrap();
        let cachedir = basedir.path().join("cache");
        let _cache = Cache::from_config(
            "test",
            Some(cachedir.clone()),
            None,
            CacheConfig::Downloaded(Default::default()),
        );
        let fsinfo = fs::metadata(cachedir).unwrap();
        assert!(fsinfo.is_dir());
    }

    #[test]
    fn test_caches_tmp_created() {
        let basedir = tempdir().unwrap();
        let cachedir = basedir.path().join("cache");
        let tmpdir = cachedir.join("tmp");

        let cfg = Config {
            cache_dir: Some(cachedir),
            ..Default::default()
        };
        let caches = Caches::from_config(&cfg).unwrap();
        caches.clear_tmp(&cfg).unwrap();

        let fsinfo = fs::metadata(tmpdir).unwrap();
        assert!(fsinfo.is_dir());
    }

    #[test]
    fn test_caches_tmp_cleared() {
        let basedir = tempdir().unwrap();
        let cachedir = basedir.path().join("cache");
        let tmpdir = cachedir.join("tmp");

        create_dir_all(&tmpdir).unwrap();
        let spam = tmpdir.join("spam");
        File::create(&spam).unwrap();
        let fsinfo = fs::metadata(&spam).unwrap();
        assert!(fsinfo.is_file());

        let cfg = Config {
            cache_dir: Some(cachedir),
            ..Default::default()
        };
        let caches = Caches::from_config(&cfg).unwrap();
        caches.clear_tmp(&cfg).unwrap();

        let fsinfo = fs::metadata(spam);
        assert!(fsinfo.is_err());
    }

    #[test]
    fn test_max_unused_for() -> Result<()> {
        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("foo"))?;

        let cache = Cache::from_config(
            "test",
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Derived(DerivedCacheConfig {
                max_unused_for: Some(Duration::from_millis(50)),
                ..Default::default()
            }),
        )?;

        File::create(tempdir.path().join("foo/killthis"))?.write_all(b"hi")?;
        File::create(tempdir.path().join("foo/keepthis"))?.write_all(b"")?;
        sleep(Duration::from_millis(100));

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
    fn test_retry_misses_after() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;
        use std::thread::sleep;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("foo"))?;

        let cache = Cache::from_config(
            "test",
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Derived(DerivedCacheConfig {
                retry_misses_after: Some(Duration::from_millis(50)),
                ..Default::default()
            }),
        )?;

        File::create(tempdir.path().join("foo/keepthis"))?.write_all(b"hi")?;
        File::create(tempdir.path().join("foo/killthis"))?.write_all(b"")?;
        sleep(Duration::from_millis(100));

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
    fn test_cleanup_malformed() -> Result<()> {
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
        let cache = Cache::from_config(
            "test",
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Derived(Default::default()),
        )?;

        cache.cleanup()?;

        let mut basenames: Vec<_> = read_dir(tempdir.path().join("foo"))?
            .map(|x| x.unwrap().file_name().into_string().unwrap())
            .collect();

        basenames.sort();

        assert_eq!(basenames, vec!["keepthis", "keepthis2"]);

        Ok(())
    }
}
