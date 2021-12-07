//! Core logic for cache files. Used by `crate::services::common::cache`.

use core::fmt;
use std::fs::{read_dir, remove_file};
use std::io::{self, Read, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicIsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use filetime::FileTime;
use serde::{Deserialize, Serialize};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::config::{CacheConfig, Config};
use crate::logging::LogError;

/// Starting content of cache items whose writing failed.
///
/// Items with this value will be considered expired after the next process restart, or will be
/// pruned once `symbolicator cleanup` runs. Independently of any `max_age` or `max_last_used`.
///
/// The malformed state is useful for failed computations that are unlikely to succeed before the
/// next deploy. For example, symcache writing may fail due to an object file symbolic can't parse
/// yet.
pub const MALFORMED_MARKER: &[u8] = b"malformed";

/// Starting content of cache items where an error has occurred, typically related to a
/// cache-specific operation. For example, this would be download errors for download caches, and
/// conversion errors for derived caches.
///
/// Items with this value will be expired after an hour.
///
/// Additional notes for download caches:
/// This state is used as a way to distinguish between the absence of a file and the failure to
/// fetch a file that is known to be present, but unfetchable due to transient errors. Absent files
/// are covered by the negative cache state.
pub const CACHE_SPECIFIC_ERROR_MARKER: &[u8] = b"cachespecificerror";

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum CacheStatus {
    /// A cache item that represents the presence of something. E.g. we succeeded in downloading an
    /// object file and cached that file.
    Positive,
    /// A cache item that represents the absence of something. E.g. we encountered a 404 or a client
    /// error while trying to download a file, and cached that fact. Represented by an empty file.
    /// For the download cache, 403 errors do not fall under `Negative` but `CacheSpecificError`.
    Negative,
    /// We are unable to create or use the cache item. E.g. we failed to create a symcache.
    /// See docs for [`MALFORMED_MARKER`].
    Malformed(String),
    /// We are unable to create the cache item due to an error exclusive to the type of cache the
    /// entry is being created for. See docs for [`CACHE_SPECIFIC_ERROR_MARKER`].
    CacheSpecificError(String),
}

impl AsRef<str> for CacheStatus {
    fn as_ref(&self) -> &str {
        match self {
            CacheStatus::Positive => "positive",
            CacheStatus::Negative => "negative",
            CacheStatus::Malformed(_) => "malformed",
            CacheStatus::CacheSpecificError(_) => "cache-specific error",
        }
    }
}

impl CacheStatus {
    pub fn from_content(s: &[u8]) -> CacheStatus {
        if s.starts_with(MALFORMED_MARKER) {
            let raw_message = s.get(MALFORMED_MARKER.len()..).unwrap_or_default();
            let err_msg = String::from_utf8_lossy(raw_message);
            CacheStatus::Malformed(err_msg.into_owned())
        } else if s.starts_with(CACHE_SPECIFIC_ERROR_MARKER) {
            let raw_message = s
                .get(CACHE_SPECIFIC_ERROR_MARKER.len()..)
                .unwrap_or_default();
            let err_msg = String::from_utf8_lossy(raw_message);
            CacheStatus::CacheSpecificError(err_msg.into_owned())
        } else if s.is_empty() {
            CacheStatus::Negative
        } else {
            CacheStatus::Positive
        }
    }

    /// Writes the status marker to the file, leaving the cursor at the end.
    ///
    /// For a positive status this only seeks to the end.  For the other cases this seeks
    /// to the beginning, writes the appropriate marker and truncates the file.
    pub async fn write(&self, file: &mut File) -> Result<(), io::Error> {
        match self {
            CacheStatus::Positive => {
                file.seek(SeekFrom::End(0)).await?;
            }
            CacheStatus::Negative => {
                file.rewind().await?;
                file.set_len(0).await?;
            }
            CacheStatus::Malformed(details) => {
                file.rewind().await?;
                file.write_all(MALFORMED_MARKER).await?;
                file.write_all(details.as_bytes()).await?;
                let new_len = file.stream_position().await?;
                file.set_len(new_len).await?;
            }
            CacheStatus::CacheSpecificError(details) => {
                file.rewind().await?;
                file.write_all(CACHE_SPECIFIC_ERROR_MARKER).await?;
                file.write_all(details.as_bytes()).await?;
                let new_len = file.stream_position().await?;
                file.set_len(new_len).await?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemSharedCacheConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcsSharedCacheConfig {
    /// Name of the GCS bucket.
    pub bucket: String,

    /// Optional name of a JSON file containing the service account credentials.
    ///
    /// If this is not provided the JSON will be looked up in the
    /// `GOOGLE_APPLICATION_CREDENTIALS` variable.  Otherwise it is assumed the service is
    /// running since GCP and will retrieve the correct user or service account from the
    /// GCP services.
    #[serde(default)]
    pub service_account_path: Option<PathBuf>,
}

/// The backend to use for the shared cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharedCacheBackendConfig {
    Gcs(GcsSharedCacheConfig),
    Filesystem(FilesystemSharedCacheConfig),
}

/// A remote cache that can be shared between symbolicator instances.
///
/// Any files not in the local cache will be looked up from here before being looked up in
/// their original source.  Additionally derived caches are also stored in here to save
/// computations if another symbolicator has already done the computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedCacheConfig {
    /// The number of allowed concurrent uploads to the shared cache.
    ///
    /// Uploading to the shared cache is not critical for symbolicator's operation and
    /// should not disrupt any normal work it does.  This limits the number of concurrent
    /// uploads so that associated resources are kept in check.
    #[serde(default = "default_max_concurrent_uploads")]
    pub max_concurrent_uploads: usize,

    /// The number of queued up uploads to the cache.
    ///
    /// If more items need to be uploaded to the shared cache than there are allowed
    /// concurrently the uploads will be queued.  If the queue is full the uploads are
    /// simply dropped as they are not critical to symbolicator's operation and not
    /// disrupting symbolicator is more important than uploading to the shared cache.
    #[serde(default = "default_max_upload_queue_size")]
    pub max_upload_queue_size: usize,

    /// The backend to use for the shared cache.
    #[serde(flatten)]
    pub backend: SharedCacheBackendConfig,
}

fn default_max_upload_queue_size() -> usize {
    400
}

fn default_max_concurrent_uploads() -> usize {
    20
}

/// All known cache names.
#[derive(Debug, Clone, Copy)]
pub enum CacheName {
    Objects,
    ObjectMeta,
    Auxdifs,
    Symcaches,
    Cficaches,
    Diagnostics,
}

impl AsRef<str> for CacheName {
    fn as_ref(&self) -> &str {
        match self {
            Self::Objects => "objects",
            Self::ObjectMeta => "object_meta",
            Self::Auxdifs => "auxdifs",
            Self::Symcaches => "symcaches",
            Self::Cficaches => "cficaches",
            Self::Diagnostics => "diagnostics",
        }
    }
}

impl fmt::Display for CacheName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

/// Common cache configuration.
///
/// Many parts of symbolicator use a cache to save having to re-download data or reprocess
/// downloaded data.  All caches behave similarly and their behaviour is determined by this
/// struct.
#[derive(Debug, Clone)]
pub struct Cache {
    /// Cache identifier used for metric names.
    name: CacheName,

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

    /// The maximum number of lazy refreshes of this cache.
    max_lazy_refreshes: Arc<AtomicIsize>,
}

impl Cache {
    pub fn from_config(
        name: CacheName,
        cache_dir: Option<PathBuf>,
        tmp_dir: Option<PathBuf>,
        cache_config: CacheConfig,
        max_lazy_refreshes: Arc<AtomicIsize>,
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
            max_lazy_refreshes,
        })
    }

    pub fn name(&self) -> CacheName {
        self.name
    }

    pub fn cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_deref()
    }

    pub fn max_lazy_refreshes(&self) -> Arc<AtomicIsize> {
        self.max_lazy_refreshes.clone()
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

        let expiration_strategy = expiration_strategy(&self.cache_config, path)?;

        if expiration_strategy == ExpirationStrategy::Malformed {
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

        let max_mtime = if expiration_strategy == ExpirationStrategy::Negative {
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

        let touch_before_use = expiration_strategy == ExpirationStrategy::None
            && mtime.map(|x| x > Duration::from_secs(3600)).unwrap_or(true);
        Ok(touch_before_use)
    }

    /// Validates `cachefile` against expiration config and open a [`ByteView`] on it.
    ///
    /// Takes care of bumping `mtime`.
    pub fn open_cachefile(&self, path: &Path) -> io::Result<Option<ByteView<'static>>> {
        // `io::ErrorKind::NotFound` can be returned from multiple locations in this function. All
        // of those can indicate a cache miss as cache cleanup can run inbetween. Only when we have
        // an open ByteView we can be sure to have a cache hit.
        catch_not_found(|| {
            let should_touch = self.check_expiry(path)?;

            if should_touch {
                filetime::set_file_mtime(path, FileTime::now())?;
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

/// Expiration strategies for cache items. These aren't named after the strategies themselves right
/// now but after the type of cache entry they should be used on instead.
#[derive(Debug, PartialEq)]
enum ExpirationStrategy {
    /// Clean up after it is untouched for a fixed period of time.
    None,
    /// Clean up after a forced cool-off period so it can be re-downloaded.
    Negative,
    /// Clean up after it is untouched for a fixed period of time. Immediately clean up if the item
    /// was last touched before the process executing cleanup started.
    Malformed,
}

/// Reads a cache item at a given path and returns the cleanup strategy that should be used
/// for the item.
fn expiration_strategy(cache_config: &CacheConfig, path: &Path) -> io::Result<ExpirationStrategy> {
    let metadata = path.metadata()?;

    let largest_sentinel = MALFORMED_MARKER
        .len()
        .max(CACHE_SPECIFIC_ERROR_MARKER.len());
    let readable_amount = largest_sentinel.min(metadata.len() as usize);
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0; readable_amount];

    file.read_exact(&mut buf)?;
    log::trace!("First {} bytes: {:?}", buf.len(), buf);

    let strategy = match CacheStatus::from_content(&buf) {
        CacheStatus::Positive => ExpirationStrategy::None,
        CacheStatus::Negative => ExpirationStrategy::Negative,
        CacheStatus::Malformed(_) => ExpirationStrategy::Malformed,
        // The nature of cache-specific errors depends on the cache type so different
        // strategies are used based on which cache's file is being assessed here.
        // This won't kick in until `CacheStatus::from_content` stops classifying
        // files with CacheSpecificError contents as Malformed.
        CacheStatus::CacheSpecificError(_) => match cache_config {
            CacheConfig::Downloaded(_) => ExpirationStrategy::Negative,
            CacheConfig::Derived(_) => ExpirationStrategy::Malformed,
            CacheConfig::Diagnostics(_) => ExpirationStrategy::None,
        },
    };
    Ok(strategy)
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
    /// Caches for object files, used by [`crate::services::objects::ObjectsActor`].
    pub objects: Cache,
    /// Caches for object metadata, used by [`crate::services::objects::ObjectsActor`].
    pub object_meta: Cache,
    /// Caches for auxiliary DIF files, used by [`crate::services::bitcode::BitcodeService`].
    pub auxdifs: Cache,
    /// Caches for [`symbolic::symcache::SymCache`], used by
    /// [`crate::services::symcaches::SymCacheActor`].
    pub symcaches: Cache,
    /// Caches for breakpad CFI info, used by [`crate::services::cficaches::CfiCacheActor`].
    pub cficaches: Cache,
    /// Store for diagnostics data symbolicator failed to process, used by
    /// [`crate::services::symbolication::SymbolicationActor`].
    pub diagnostics: Cache,
}

impl Caches {
    pub fn from_config(config: &Config) -> io::Result<Self> {
        // The minimum value here is clamped to 1, as it would otherwise completely disable lazy
        // re-generation. We might as well decide to hard `panic!` on startup if users have
        // misconfigured this instead of silently correcting it to a value that actually makes sense.
        let max_lazy_redownloads = Arc::new(AtomicIsize::new(
            config.caches.downloaded.max_lazy_redownloads.max(1),
        ));
        let max_lazy_recomputations = Arc::new(AtomicIsize::new(
            config.caches.derived.max_lazy_recomputations.max(1),
        ));

        let tmp_dir = config.cache_dir("tmp");
        Ok(Self {
            objects: {
                let path = config.cache_dir("objects");
                Cache::from_config(
                    CacheName::Objects,
                    path,
                    tmp_dir.clone(),
                    config.caches.downloaded.into(),
                    max_lazy_redownloads.clone(),
                )?
            },
            object_meta: {
                let path = config.cache_dir("object_meta");
                Cache::from_config(
                    CacheName::ObjectMeta,
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                    max_lazy_recomputations.clone(),
                )?
            },
            auxdifs: {
                let path = config.cache_dir("auxdifs");
                Cache::from_config(
                    CacheName::Auxdifs,
                    path,
                    tmp_dir.clone(),
                    config.caches.downloaded.into(),
                    max_lazy_redownloads,
                )?
            },
            symcaches: {
                let path = config.cache_dir("symcaches");
                Cache::from_config(
                    CacheName::Symcaches,
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                    max_lazy_recomputations.clone(),
                )?
            },
            cficaches: {
                let path = config.cache_dir("cficaches");
                Cache::from_config(
                    CacheName::Cficaches,
                    path,
                    tmp_dir.clone(),
                    config.caches.derived.into(),
                    max_lazy_recomputations,
                )?
            },
            diagnostics: {
                let path = config.cache_dir("diagnostics");
                Cache::from_config(
                    CacheName::Diagnostics,
                    path,
                    tmp_dir,
                    config.caches.diagnostics.into(),
                    Default::default(),
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
        // Destructure so we do not accidentally forget to cleanup one of our members.
        let Self {
            objects,
            object_meta,
            auxdifs,
            symcaches,
            cficaches,
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
        ];

        let mut first_error = None;
        for result in results {
            if let Err(err) = result {
                let stderr: &dyn std::error::Error = &*err;
                log::error!("Failed to cleanup cache: {}", LogError(stderr));
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

/// Entry function for the cleanup command.
///
/// This will clean up all caches based on configured cache retention.
pub fn cleanup(config: Config) -> Result<()> {
    Caches::from_config(&config)?.cleanup()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::convert::TryInto;
    use std::fs::{self, create_dir_all, File};
    use std::io::Write;
    use std::thread::sleep;

    use crate::config::{
        CacheConfigs, DerivedCacheConfig, DiagnosticsCacheConfig, DownloadedCacheConfig,
    };

    fn tempdir() -> io::Result<tempfile::TempDir> {
        tempfile::tempdir_in(".")
    }

    #[test]
    fn test_cache_dir_created() {
        let basedir = tempdir().unwrap();
        let cachedir = basedir.path().join("cache");
        let _cache = Cache::from_config(
            CacheName::Objects,
            Some(cachedir.clone()),
            None,
            CacheConfig::Downloaded(Default::default()),
            Default::default(),
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
            CacheName::Objects,
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Derived(DerivedCacheConfig {
                max_unused_for: Some(Duration::from_millis(50)),
                ..Default::default()
            }),
            Default::default(),
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
            CacheName::Objects,
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Derived(DerivedCacheConfig {
                retry_misses_after: Some(Duration::from_millis(50)),
                ..Default::default()
            }),
            Default::default(),
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
        File::create(tempdir.path().join("foo/keepthis3"))?.write_all(b"honkhonkbeepbeep")?;

        File::create(tempdir.path().join("foo/killthis"))?.write_all(b"malformed")?;
        File::create(tempdir.path().join("foo/killthis2"))?.write_all(b"malformedhonk")?;

        sleep(Duration::from_millis(10));

        // Creation of this struct == "process startup", this tests that all malformed files created
        // before startup are cleaned
        let cache = Cache::from_config(
            CacheName::Objects,
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Derived(DerivedCacheConfig {
                retry_misses_after: Some(Duration::from_millis(20)),
                ..Default::default()
            }),
            Default::default(),
        )?;

        cache.cleanup()?;

        let mut basenames: Vec<_> = read_dir(tempdir.path().join("foo"))?
            .map(|x| x.unwrap().file_name().into_string().unwrap())
            .collect();

        basenames.sort();

        assert_eq!(basenames, vec!["keepthis", "keepthis2", "keepthis3"]);

        Ok(())
    }

    #[test]
    fn test_cleanup_cache_specific_error_derived() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;
        use std::thread::sleep;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("foo"))?;

        File::create(tempdir.path().join("foo/keepthis"))?.write_all(b"beeep")?;
        File::create(tempdir.path().join("foo/keepthis2"))?.write_all(b"hi")?;
        File::create(tempdir.path().join("foo/keepthis3"))?.write_all(b"honkhonkbeepbeep")?;

        File::create(tempdir.path().join("foo/killthis"))?.write_all(b"cachespecificerror")?;
        File::create(tempdir.path().join("foo/killthis2"))?.write_all(b"cachespecificerrorhonk")?;
        File::create(tempdir.path().join("foo/killthis3"))?
            .write_all(b"cachespecificerrormalformed")?;
        File::create(tempdir.path().join("foo/killthis4"))?
            .write_all(b"malformedcachespecificerror")?;

        // Creation of this struct == "process startup", this tests that all cache-specific error files created
        // before startup are cleaned
        let cache = Cache::from_config(
            CacheName::Objects,
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Derived(DerivedCacheConfig {
                retry_misses_after: Some(Duration::from_millis(20)),
                ..Default::default()
            }),
            Default::default(),
        )?;

        sleep(Duration::from_millis(30));

        cache.cleanup()?;

        let mut basenames: Vec<_> = read_dir(tempdir.path().join("foo"))?
            .map(|x| x.unwrap().file_name().into_string().unwrap())
            .collect();

        basenames.sort();

        assert_eq!(basenames, vec!["keepthis", "keepthis2", "keepthis3"]);

        Ok(())
    }

    #[test]
    fn test_cleanup_cache_specific_error_download() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;
        use std::thread::sleep;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("foo"))?;

        File::create(tempdir.path().join("foo/keepthis"))?.write_all(b"beeep")?;
        File::create(tempdir.path().join("foo/keepthis2"))?.write_all(b"hi")?;
        File::create(tempdir.path().join("foo/keepthis3"))?.write_all(b"honkhonkbeepbeep")?;

        File::create(tempdir.path().join("foo/killthis"))?.write_all(b"cachespecificerror")?;
        File::create(tempdir.path().join("foo/killthis2"))?.write_all(b"cachespecificerrorhonk")?;
        File::create(tempdir.path().join("foo/killthis3"))?
            .write_all(b"cachespecificerrormalformed")?;
        File::create(tempdir.path().join("foo/killthis4"))?
            .write_all(b"malformedcachespecificerror")?;

        let cache = Cache::from_config(
            CacheName::Objects,
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Downloaded(DownloadedCacheConfig {
                retry_misses_after: Some(Duration::from_millis(20)),
                ..Default::default()
            }),
            Default::default(),
        )?;

        sleep(Duration::from_millis(30));

        cache.cleanup()?;

        let mut basenames: Vec<_> = read_dir(tempdir.path().join("foo"))?
            .map(|x| x.unwrap().file_name().into_string().unwrap())
            .collect();

        basenames.sort();

        assert_eq!(basenames, vec!["keepthis", "keepthis2", "keepthis3"]);

        Ok(())
    }

    #[test]
    fn test_expiration_strategy_positive() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("honk"))?;

        File::create(tempdir.path().join("honk/keepbeep"))?.write_all(b"toot")?;
        File::create(tempdir.path().join("honk/keepbeep2"))?.write_all(b"honk")?;
        File::create(tempdir.path().join("honk/keepbeep3"))?.write_all(b"honkhonkbeepbeep")?;
        File::create(tempdir.path().join("honk/keepbeep4"))?.write_all(b"malform")?;
        File::create(tempdir.path().join("honk/keepbeep5"))?.write_all(b"dler")?;

        let cache_configs = vec![
            CacheConfig::Downloaded(Default::default()),
            CacheConfig::Derived(Default::default()),
            CacheConfig::Diagnostics(Default::default()),
        ];

        for config in cache_configs {
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/keepbeep").as_path())?,
                ExpirationStrategy::None,
            );
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/keepbeep2").as_path())?,
                ExpirationStrategy::None,
            );
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/keepbeep3").as_path())?,
                ExpirationStrategy::None,
            );
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/keepbeep4").as_path())?,
                ExpirationStrategy::None,
            );
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/keepbeep5").as_path())?,
                ExpirationStrategy::None,
            );
        }

        Ok(())
    }

    #[test]
    fn test_expiration_strategy_negative() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("honk"))?;

        File::create(tempdir.path().join("honk/retrybeep"))?.write_all(b"")?;

        let cache_configs = vec![
            CacheConfig::Downloaded(Default::default()),
            CacheConfig::Derived(Default::default()),
            CacheConfig::Diagnostics(Default::default()),
        ];

        for config in cache_configs {
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/retrybeep").as_path())?,
                ExpirationStrategy::Negative,
            );
        }

        Ok(())
    }

    #[test]
    fn test_expiration_strategy_malformed() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("honk"))?;

        File::create(tempdir.path().join("honk/badbeep"))?.write_all(b"malformed")?;

        File::create(tempdir.path().join("honk/badbeep2"))?.write_all(b"malformedhonkbeep")?;

        File::create(tempdir.path().join("honk/badbeep3"))?
            .write_all(b"malformedcachespecificerror")?;

        let cache_configs = vec![
            CacheConfig::Downloaded(Default::default()),
            CacheConfig::Derived(Default::default()),
            CacheConfig::Diagnostics(Default::default()),
        ];

        for config in cache_configs {
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/badbeep").as_path())?,
                ExpirationStrategy::Malformed,
            );
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/badbeep2").as_path())?,
                ExpirationStrategy::Malformed,
            );
            assert_eq!(
                expiration_strategy(&config, tempdir.path().join("honk/badbeep3").as_path())?,
                ExpirationStrategy::Malformed,
            );
        }
        Ok(())
    }

    #[test]
    fn test_expiration_strategy_cache_specific_err_derived() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("honk"))?;

        File::create(tempdir.path().join("honk/badbeep"))?.write_all(b"cachespecificerror")?;

        File::create(tempdir.path().join("honk/badbeep2"))?
            .write_all(b"cachespecificerrorhonkbeep")?;

        File::create(tempdir.path().join("honk/badbeep3"))?
            .write_all(b"cachespecificerrormalformed")?;

        let config = CacheConfig::Derived(Default::default());

        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep").as_path())?,
            ExpirationStrategy::Malformed,
        );
        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep2").as_path())?,
            ExpirationStrategy::Malformed,
        );
        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep3").as_path())?,
            ExpirationStrategy::Malformed,
        );
        Ok(())
    }

    #[test]
    fn test_expiration_strategy_cache_specific_err_diagnostics() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("honk"))?;

        File::create(tempdir.path().join("honk/badbeep"))?.write_all(b"cachespecificerror")?;

        File::create(tempdir.path().join("honk/badbeep2"))?
            .write_all(b"cachespecificerrorhonkbeep")?;

        File::create(tempdir.path().join("honk/badbeep3"))?
            .write_all(b"cachespecificerrormalformed")?;

        let config = CacheConfig::Diagnostics(Default::default());

        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep").as_path())?,
            ExpirationStrategy::None,
        );
        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep2").as_path())?,
            ExpirationStrategy::None,
        );
        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep3").as_path())?,
            ExpirationStrategy::None,
        );
        Ok(())
    }

    #[test]
    fn test_expiration_strategy_cache_specific_err_downloaded() -> Result<()> {
        use std::fs::create_dir_all;
        use std::io::Write;

        let tempdir = tempdir()?;
        create_dir_all(tempdir.path().join("honk"))?;

        File::create(tempdir.path().join("honk/badbeep"))?.write_all(b"cachespecificerror")?;

        File::create(tempdir.path().join("honk/badbeep2"))?
            .write_all(b"cachespecificerrorhonkbeep")?;

        File::create(tempdir.path().join("honk/badbeep3"))?
            .write_all(b"cachespecificerrormalformed")?;

        let config = CacheConfig::Downloaded(Default::default());

        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep").as_path())?,
            ExpirationStrategy::Negative,
        );
        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep2").as_path())?,
            ExpirationStrategy::Negative,
        );
        assert_eq!(
            expiration_strategy(&config, tempdir.path().join("honk/badbeep3").as_path())?,
            ExpirationStrategy::Negative,
        );
        Ok(())
    }

    #[test]
    fn test_open_cachefile() -> Result<()> {
        // Assert that opening a cache touches the mtime but does not invalidate it.
        let tempdir = tempdir()?;
        let cache = Cache::from_config(
            CacheName::Objects,
            Some(tempdir.path().to_path_buf()),
            None,
            CacheConfig::Downloaded(Default::default()),
            Default::default(),
        )?;

        // Create a file in the cache, with mtime of 1h 15s ago since it only gets touched
        // if more than an hour old.
        let path = tempdir.path().join("hello");
        File::create(&path)?.write_all(b"world")?;
        let now_unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        let old_mtime_unix = (now_unix - 3600 - 15).try_into()?;
        filetime::set_file_mtime(&path, FileTime::from_unix_time(old_mtime_unix, 0))?;

        let old_mtime = fs::metadata(&path)?.modified()?;

        // Open it with the cache, check contents and new mtime.
        let view = cache.open_cachefile(&path)?.expect("No file found");
        assert_eq!(view.as_slice(), b"world");

        let new_mtime = fs::metadata(&path)?.modified()?;
        assert!(old_mtime < new_mtime);

        Ok(())
    }

    #[test]
    fn test_cleanup() {
        let tempdir = tempdir().unwrap();

        // Create entries in our caches that are an hour old.
        let mtime = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(3600));

        let create = |cache_name: &str| {
            let dir = tempdir.path().join(cache_name);
            fs::create_dir(&dir).unwrap();
            let entry = dir.join("entry");
            fs::write(&entry, "contents").unwrap();
            filetime::set_file_mtime(&entry, mtime).unwrap();
            entry
        };

        let object_entry = create("objects");
        let object_meta_entry = create("object_meta");
        let auxdifs_entry = create("auxdifs");
        let symcaches_entry = create("symcaches");
        let cficaches_entry = create("cficaches");
        let diagnostics_entry = create("diagnostics");

        // Configure the caches to expire after 1 minute.
        let caches = Caches::from_config(&Config {
            cache_dir: Some(tempdir.path().to_path_buf()),
            caches: CacheConfigs {
                downloaded: DownloadedCacheConfig {
                    max_unused_for: Some(Duration::from_secs(60)),
                    ..Default::default()
                },
                derived: DerivedCacheConfig {
                    max_unused_for: Some(Duration::from_secs(60)),
                    ..Default::default()
                },
                diagnostics: DiagnosticsCacheConfig {
                    retention: Some(Duration::from_secs(60)),
                },
            },
            ..Default::default()
        })
        .unwrap();

        // Finally do some testing
        assert!(object_entry.is_file());
        assert!(object_meta_entry.is_file());
        assert!(auxdifs_entry.is_file());
        assert!(symcaches_entry.is_file());
        assert!(cficaches_entry.is_file());
        assert!(diagnostics_entry.is_file());

        caches.cleanup().unwrap();

        assert!(!object_entry.is_file());
        assert!(!object_meta_entry.is_file());
        assert!(!auxdifs_entry.is_file());
        assert!(!symcaches_entry.is_file());
        assert!(!cficaches_entry.is_file());
        assert!(!diagnostics_entry.is_file());
    }

    #[tokio::test]
    async fn test_cache_status_write_positive() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);

        async_file.write_all(b"beep").await?;
        // moving the cursor back to the beginning so the cursor behaviour
        // is checked
        async_file.rewind().await?;

        let status = CacheStatus::Positive;
        status.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(current_pos, 4);

        let contents = fs::read(&path)?;
        assert_eq!(contents, b"beep");

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_status_write_negative() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);
        let status = CacheStatus::Negative;
        status.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(current_pos, 0);

        let contents = fs::read(&path)?;
        assert_eq!(contents, b"");

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_status_write_negative_with_garbage() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);
        async_file.write_all(b"beep").await?;
        let status = CacheStatus::Negative;
        status.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(current_pos, 0);

        let contents = fs::read(&path)?;
        assert_eq!(contents, b"");

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_status_write_malformed() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);

        let error_message = "unsupported object file format";
        let status = CacheStatus::Malformed(error_message.to_owned());
        status.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(
            current_pos as usize,
            MALFORMED_MARKER.len() + error_message.len()
        );

        let contents = fs::read(&path)?;

        let mut expected: Vec<u8> = Vec::new();
        expected.extend(MALFORMED_MARKER);
        expected.extend(error_message.as_bytes());

        assert_eq!(contents, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_status_write_malformed_truncates() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);

        async_file
            .write_all(
                b"i'm a little teapot short and stout here is my handle and here is my spout",
            )
            .await?;

        let error_message = "unsupported object file format";
        let status = CacheStatus::Malformed(error_message.to_owned());
        status.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(
            current_pos as usize,
            MALFORMED_MARKER.len() + error_message.len()
        );

        let contents = fs::read(&path)?;

        let mut expected: Vec<u8> = Vec::new();
        expected.extend(MALFORMED_MARKER);
        expected.extend(error_message.as_bytes());

        assert_eq!(contents, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_status_write_cache_error() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);

        let error_message = "missing permissions for file";
        let status = CacheStatus::CacheSpecificError(error_message.to_owned());
        status.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(
            current_pos as usize,
            CACHE_SPECIFIC_ERROR_MARKER.len() + error_message.len()
        );

        let contents = fs::read(&path)?;

        let mut expected: Vec<u8> = Vec::new();
        expected.extend(CACHE_SPECIFIC_ERROR_MARKER);
        expected.extend(error_message.as_bytes());

        assert_eq!(contents, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_status_write_cache_error_truncates() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);

        async_file
            .write_all(
                b"i'm a little teapot short and stout here is my handle and here is my spout",
            )
            .await?;

        let error_message = "missing permissions for file";
        let status = CacheStatus::CacheSpecificError(error_message.to_owned());
        status.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(
            current_pos as usize,
            CACHE_SPECIFIC_ERROR_MARKER.len() + error_message.len()
        );

        let contents = fs::read(&path)?;

        let mut expected: Vec<u8> = Vec::new();
        expected.extend(CACHE_SPECIFIC_ERROR_MARKER);
        expected.extend(error_message.as_bytes());

        assert_eq!(contents, expected);

        Ok(())
    }
    #[test]
    fn test_shared_cache_config_filesystem_common_defaults() {
        let yaml = r#"
            filesystem:
              path: "/path/to/somewhere"
        "#;
        let cfg: SharedCacheConfig = serde_yaml::from_reader(yaml.as_bytes()).unwrap();

        assert_eq!(cfg.max_upload_queue_size, 400);
        assert_eq!(cfg.max_concurrent_uploads, 20);
        match cfg.backend {
            SharedCacheBackendConfig::Gcs(_) => panic!("wrong backend"),
            SharedCacheBackendConfig::Filesystem(cfg) => {
                assert_eq!(cfg.path, Path::new("/path/to/somewhere"))
            }
        }
    }

    #[test]
    fn test_shared_cache_config_common_settings() {
        let yaml = r#"
            max_upload_queue_size: 50
            max_concurrent_uploads: 50
            filesystem:
              path: "/path/to/somewhere"
        "#;
        let cfg: SharedCacheConfig = serde_yaml::from_reader(yaml.as_bytes()).unwrap();

        assert_eq!(cfg.max_upload_queue_size, 50);
        assert_eq!(cfg.max_concurrent_uploads, 50);
        assert!(matches!(
            cfg.backend,
            SharedCacheBackendConfig::Filesystem(_)
        ));
    }

    #[test]
    fn test_shared_cache_config_gcs() {
        let yaml = r#"
            gcs:
              bucket: "some-bucket"
        "#;
        let cfg: SharedCacheConfig = serde_yaml::from_reader(yaml.as_bytes()).unwrap();

        match cfg.backend {
            SharedCacheBackendConfig::Gcs(gcs) => {
                assert_eq!(gcs.bucket, "some-bucket");
                assert!(gcs.service_account_path.is_none());
            }
            SharedCacheBackendConfig::Filesystem(_) => panic!("wrong backend"),
        }
    }
}
