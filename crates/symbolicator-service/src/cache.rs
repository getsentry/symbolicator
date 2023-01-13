//! Core logic for cache files. Used by `crate::services::common::cache`.

use core::fmt;
use std::fs::{read_dir, remove_dir, remove_file};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicIsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use filetime::FileTime;
use humantime_serde::re::humantime::{format_duration, parse_duration};
use serde::{Deserialize, Serialize};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::config::{CacheConfig, Config};

/// An error that happens when fetching an object from a remote location.
///
/// This error enum is intended for persisting in caches, except for the
/// [`InternalError`](Self::InternalError) variant.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CacheError {
    /// The object was not found at the remote source.
    #[error("not found")]
    NotFound,
    /// The object could not be fetched from the remote source due to missing
    /// permissions.
    ///
    /// The attached string contains the remote source's response.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The object could not be fetched from the remote source due to a timeout.
    #[error("download timed out")]
    Timeout(Duration),
    /// The object could not be fetched from the remote source due to another problem,
    /// like connection loss, DNS resolution, or a 5xx server response.
    ///
    /// The attached string contains the remote source's response.
    #[error("download failed: {0}")]
    DownloadError(String),
    /// The object was fetched successfully, but is invalid in some way.
    ///
    /// For example, this could result from an unsupported object file or an error
    /// during symcache conversion
    #[error("malformed: {0}")]
    Malformed(String),
    /// An unexpected error in symbolicator itself.
    ///
    /// This variant is not intended to be persisted to or read from caches.
    #[error("internal error")]
    InternalError,
}

impl From<std::io::Error> for CacheError {
    #[track_caller]
    fn from(err: std::io::Error) -> Self {
        Self::from_std_error(err)
    }
}

impl From<serde_json::Error> for CacheError {
    #[track_caller]
    fn from(err: serde_json::Error) -> Self {
        Self::from_std_error(err)
    }
}

impl CacheError {
    const MALFORMED_MARKER: &[u8] = b"malformed";
    const PERMISSION_DENIED_MARKER: &[u8] = b"permissiondenied";
    const TIMEOUT_MARKER: &[u8] = b"timeout";
    const DOWNLOAD_ERROR_MARKER: &[u8] = b"downloaderror";

    const SHOULD_WRITE_LEGACY_MARKERS: bool = true;

    // These markers were used in the older `CacheStatus` implementation:
    const LEGACY_PERMISSION_DENIED_MARKER: &[u8] =
        b"cachespecificerrormissing permissions for file";
    const LEGACY_TIMEOUT_MARKER: &[u8] = b"cachespecificerrordownload was cancelled";
    const LEGACY_DOWNLOAD_ERROR_MARKER: &[u8] = b"cachespecificerror";

    /// Writes error markers and details to a file.
    ///
    /// * If `self` is [`InternalError`](Self::InternalError), it does nothing.
    /// * If `self` is [`NotFound`](Self::NotFound), it empties the file.
    /// * In all other cases, it writes the corresponding marker, followed by the error
    /// details, and truncates the file.
    pub async fn write(&self, file: &mut File) -> Result<(), io::Error> {
        if let Self::InternalError = self {
            tracing::error!("A `CacheError::InternalError` should never be written out");
            return Ok(());
        }
        file.rewind().await?;

        match self {
            CacheError::NotFound => {
                // NOOP
            }
            CacheError::Malformed(details) => {
                file.write_all(Self::MALFORMED_MARKER).await?;
                file.write_all(details.as_bytes()).await?;
            }
            CacheError::PermissionDenied(details) => {
                if Self::SHOULD_WRITE_LEGACY_MARKERS {
                    file.write_all(Self::LEGACY_PERMISSION_DENIED_MARKER)
                        .await?;
                } else {
                    file.write_all(Self::PERMISSION_DENIED_MARKER).await?;
                    file.write_all(details.as_bytes()).await?;
                }
            }
            CacheError::Timeout(duration) => {
                if Self::SHOULD_WRITE_LEGACY_MARKERS {
                    file.write_all(Self::LEGACY_TIMEOUT_MARKER).await?;
                } else {
                    file.write_all(Self::TIMEOUT_MARKER).await?;
                    file.write_all(format_duration(*duration).to_string().as_bytes())
                        .await?;
                }
            }
            CacheError::DownloadError(details) => {
                if Self::SHOULD_WRITE_LEGACY_MARKERS {
                    file.write_all(Self::LEGACY_DOWNLOAD_ERROR_MARKER).await?;
                } else {
                    file.write_all(Self::DOWNLOAD_ERROR_MARKER).await?;
                }
                file.write_all(details.as_bytes()).await?;
            }
            CacheError::InternalError => {
                unreachable!("this was already handled above");
            }
        }

        let new_len = file.stream_position().await?;
        file.set_len(new_len).await?;

        Ok(())
    }

    /// Parses a `CacheError` from a byte slice.
    ///
    /// * If the slice starts with an error marker, the corresponding error variant will be returned.
    /// * If the slice is empty, [`NotFound`](Self::NotFound) will be returned.
    /// * Otherwise `None` is returned.
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if let Some(raw_message) = bytes.strip_prefix(Self::PERMISSION_DENIED_MARKER) {
            let err_msg = String::from_utf8_lossy(raw_message);
            Some(Self::PermissionDenied(err_msg.into_owned()))
        } else if let Some(raw_message) = bytes.strip_prefix(Self::LEGACY_PERMISSION_DENIED_MARKER)
        {
            // NOTE: `raw_message` is empty
            let err_msg = String::from_utf8_lossy(raw_message);
            Some(Self::PermissionDenied(err_msg.into_owned()))
        } else if let Some(_raw_duration) = bytes.strip_prefix(Self::LEGACY_TIMEOUT_MARKER) {
            // NOTE: `raw_duration` is empty
            Some(Self::Timeout(Duration::from_secs(0)))
        } else if let Some(raw_duration) = bytes.strip_prefix(Self::TIMEOUT_MARKER) {
            let raw_duration = String::from_utf8_lossy(raw_duration);
            match parse_duration(&raw_duration) {
                Ok(duration) => Some(Self::Timeout(duration)),
                Err(e) => {
                    tracing::error!(error = %e, "Failed to read timeout duration");
                    Some(Self::InternalError)
                }
            }
        } else if let Some(raw_message) = bytes.strip_prefix(Self::LEGACY_DOWNLOAD_ERROR_MARKER) {
            let err_msg = String::from_utf8_lossy(raw_message);
            Some(Self::DownloadError(err_msg.into_owned()))
        } else if let Some(raw_message) = bytes.strip_prefix(Self::DOWNLOAD_ERROR_MARKER) {
            let err_msg = String::from_utf8_lossy(raw_message);
            Some(Self::DownloadError(err_msg.into_owned()))
        } else if let Some(raw_message) = bytes.strip_prefix(Self::MALFORMED_MARKER) {
            let err_msg = String::from_utf8_lossy(raw_message);
            Some(Self::Malformed(err_msg.into_owned()))
        } else if bytes.is_empty() {
            Some(Self::NotFound)
        } else {
            None
        }
    }

    #[track_caller]
    pub(crate) fn from_std_error<E: std::error::Error + 'static>(e: E) -> Self {
        let dynerr: &dyn std::error::Error = &e; // tracing expects a `&dyn Error`
        tracing::error!(error = dynerr);
        Self::InternalError
    }
}

/// An entry in a cache, containing either `Ok(T)` or an error denoting the reason why an
/// object could not be fetched or is otherwise unusable.
pub type CacheEntry<T = ()> = Result<T, CacheError>;

/// Parses a [`CacheEntry`] from a [`ByteView`](ByteView).
pub fn cache_entry_from_bytes(bytes: ByteView<'static>) -> CacheEntry<ByteView<'static>> {
    CacheError::from_bytes(&bytes).map(Err).unwrap_or(Ok(bytes))
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
    Il2cpp,
    Symcaches,
    Cficaches,
    PpdbCaches,
    Diagnostics,
}

impl AsRef<str> for CacheName {
    fn as_ref(&self) -> &str {
        match self {
            Self::Objects => "objects",
            Self::ObjectMeta => "object_meta",
            Self::Auxdifs => "auxdifs",
            Self::Il2cpp => "il2cpp",
            Self::Symcaches => "symcaches",
            Self::Cficaches => "cficaches",
            Self::PpdbCaches => "ppdb_caches",
            Self::Diagnostics => "diagnostics",
        }
    }
}

impl fmt::Display for CacheName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

/// The interval in which positive caches should be touched.
///
/// Positive "good" caches use a "time to idle" instead of "time to live" mode.
/// We thus need to regularly "touch" the files to signal that they are still in use.
/// This is being debounced to once every hour to not have to touch them on every single use.
const TOUCH_EVERY: Duration = Duration::from_secs(3600);

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

    /// Validate cache expiration of path.
    ///
    /// If cache should not be used, `Err(io::ErrorKind::NotFound)` is returned.
    /// If cache is usable, `Ok(x)` is returned with the opened [`ByteView`], and
    /// an [`ExpirationTime`] that indicates whether the file should be touched before using.
    fn check_expiry(
        &self,
        path: &Path,
    ) -> io::Result<(CacheEntry<ByteView<'static>>, ExpirationTime)> {
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
        tracing::trace!("File length: {}", metadata.len());

        let bv = ByteView::open(path)?;
        let mtime = metadata.modified()?;
        let mtime_elapsed = mtime.elapsed().unwrap_or_default();

        let cache_entry = cache_entry_from_bytes(bv);
        let expiration_strategy = expiration_strategy(&self.cache_config, &cache_entry);

        let expiration_time = match expiration_strategy {
            ExpirationStrategy::None => {
                let max_unused_for = self.cache_config.max_unused_for().unwrap_or(Duration::MAX);

                if mtime_elapsed > max_unused_for {
                    return Err(io::ErrorKind::NotFound.into());
                }

                // we want to touch good caches once every `TOUCH_EVERY`
                let touch_in = TOUCH_EVERY.saturating_sub(mtime_elapsed);
                ExpirationTime::TouchIn(touch_in)
            }
            ExpirationStrategy::Negative => {
                let retry_misses_after = self
                    .cache_config
                    .retry_misses_after()
                    .unwrap_or(Duration::MAX);

                let expires_in = retry_misses_after.saturating_sub(mtime_elapsed);

                if expires_in == Duration::ZERO {
                    return Err(io::ErrorKind::NotFound.into());
                }

                ExpirationTime::RefreshIn(expires_in)
            }
            ExpirationStrategy::Malformed => {
                let retry_malformed_after = self
                    .cache_config
                    .retry_malformed_after()
                    .unwrap_or(Duration::MAX);

                let expires_in = retry_malformed_after.saturating_sub(mtime_elapsed);

                // Immediately expire malformed items that have been created before this process started.
                // See docstring of MALFORMED_MARKER
                if mtime < self.start_time || expires_in == Duration::ZERO {
                    tracing::trace!("Created at is older than start time");
                    return Err(io::ErrorKind::NotFound.into());
                }

                ExpirationTime::RefreshIn(expires_in)
            }
        };

        Ok((cache_entry, expiration_time))
    }

    /// Validates `cachefile` against expiration config and open a [`ByteView`] on it.
    ///
    /// Takes care of bumping `mtime`.
    ///
    /// If an open [`ByteView`] is returned it also returns whether the mtime has been
    /// bumped.
    pub fn open_cachefile(
        &self,
        path: &Path,
    ) -> io::Result<Option<(CacheEntry<ByteView<'static>>, ExpirationTime)>> {
        // `io::ErrorKind::NotFound` can be returned from multiple locations in this function. All
        // of those can indicate a cache miss as cache cleanup can run inbetween. Only when we have
        // an open ByteView we can be sure to have a cache hit.
        catch_not_found(|| {
            let (cache_entry, mut expiration) = self.check_expiry(path)?;

            let should_touch = matches!(expiration, ExpirationTime::TouchIn(Duration::ZERO));
            if should_touch {
                filetime::set_file_mtime(path, FileTime::now())?;
                // well, we just touched the file ;-)
                expiration = ExpirationTime::TouchIn(TOUCH_EVERY);
            }

            Ok((cache_entry, expiration))
        })
    }

    /// Create a new temporary file to use in the cache.
    pub fn tempfile(&self) -> io::Result<NamedTempFile> {
        match self.tmp_dir {
            Some(ref path) => {
                // The `cleanup` process could potentially remove the parent directories we are
                // operating in, so be defensive here and retry the fs operations.
                const MAX_RETRIES: usize = 2;
                let mut retries = 0;
                loop {
                    retries += 1;

                    if let Err(e) = std::fs::create_dir_all(path) {
                        sentry::with_scope(
                            |scope| scope.set_extra("path", path.display().to_string().into()),
                            || tracing::error!("Failed to create cache directory: {:?}", e),
                        );
                        if retries > MAX_RETRIES {
                            return Err(e);
                        }
                        continue;
                    }

                    match tempfile::Builder::new().prefix("tmp").tempfile_in(path) {
                        Ok(temp_file) => return Ok(temp_file),
                        Err(e) => {
                            sentry::with_scope(
                                |scope| scope.set_extra("path", path.display().to_string().into()),
                                || tracing::error!("Failed to create cache file: {:?}", e),
                            );
                            if retries > MAX_RETRIES {
                                return Err(e);
                            }
                            continue;
                        }
                    }
                }
            }
            None => Ok(NamedTempFile::new()?),
        }
    }
}

/// Expiration strategies for cache items. These aren't named after the strategies themselves right
/// now but after the type of cache entry they should be used on instead.
#[derive(Debug, PartialEq, Eq)]
pub enum ExpirationStrategy {
    /// Clean up after it is untouched for a fixed period of time.
    None,
    /// Clean up after a forced cool-off period so it can be re-downloaded.
    Negative,
    /// Clean up after it is untouched for a fixed period of time. Immediately clean up if the item
    /// was last touched before the process executing cleanup started.
    Malformed,
}

/// This gives the time at which different cache items need to be refreshed or touched.
pub enum ExpirationTime {
    /// The [`Duration`] after which [`Negative`](ExpirationStrategy::Negative) or
    /// [`Malformed`](ExpirationStrategy::Malformed) cache entries expire and need
    /// to be refreshed.
    RefreshIn(Duration),

    /// The [`Duration`] after which a positive cache entry needs to be touched to keep it
    /// alive for a longer time.
    TouchIn(Duration),
}

impl ExpirationTime {
    /// Gives the [`ExpirationTime`] for a freshly created cache with the given [`CacheEntry`].
    pub fn for_fresh_status(cache: &Cache, entry: &CacheEntry<ByteView<'static>>) -> Self {
        let config = &cache.cache_config;
        let strategy = expiration_strategy(config, entry);
        match strategy {
            ExpirationStrategy::None => {
                // we want to touch good caches once every hour
                Self::TouchIn(Duration::from_secs(3600))
            }
            ExpirationStrategy::Negative => {
                let retry_misses_after = config.retry_misses_after().unwrap_or(Duration::MAX);

                Self::RefreshIn(retry_misses_after)
            }
            ExpirationStrategy::Malformed => {
                let retry_malformed_after = config.retry_malformed_after().unwrap_or(Duration::MAX);

                Self::RefreshIn(retry_malformed_after)
            }
        }
    }

    /// Says whether the cache was just touched.
    pub fn was_touched(&self) -> bool {
        matches!(self, ExpirationTime::TouchIn(TOUCH_EVERY))
    }
}

/// Checks the cache contents in `buf` and returns the cleanup strategy that should be used
/// for the item.
fn expiration_strategy(
    cache_config: &CacheConfig,
    status: &CacheEntry<ByteView<'static>>,
) -> ExpirationStrategy {
    match status {
        Ok(_) => ExpirationStrategy::None,
        Err(CacheError::NotFound) => ExpirationStrategy::Negative,
        Err(CacheError::Malformed(_)) => ExpirationStrategy::Malformed,
        // The nature of cache-specific errors depends on the cache type so different
        // strategies are used based on which cache's file is being assessed here.
        // This won't kick in until `CacheStatus::from_content` stops classifying
        // files with CacheSpecificError contents as Malformed.
        _ => match cache_config {
            CacheConfig::Downloaded(_) => ExpirationStrategy::Negative,
            CacheConfig::Derived(_) => ExpirationStrategy::Malformed,
            CacheConfig::Diagnostics(_) => ExpirationStrategy::None,
        },
    }
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
    /// Caches for il2cpp line mapping files, used by [`crate::services::il2cpp::Il2cppService`].
    pub il2cpp: Cache,
    /// Caches for [`symbolic::symcache::SymCache`], used by
    /// [`crate::services::symcaches::SymCacheActor`].
    pub symcaches: Cache,
    /// Caches for breakpad CFI info, used by [`crate::services::cficaches::CfiCacheActor`].
    pub cficaches: Cache,
    pub ppdb_caches: Cache,
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
                    max_lazy_redownloads.clone(),
                )?
            },
            il2cpp: {
                let path = config.cache_dir("il2cpp");
                Cache::from_config(
                    CacheName::Il2cpp,
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
                    max_lazy_recomputations.clone(),
                )?
            },
            ppdb_caches: {
                let path = config.cache_dir("ppdb_caches");
                Cache::from_config(
                    CacheName::PpdbCaches,
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
            il2cpp,
            symcaches,
            cficaches,
            ppdb_caches,
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

    fn expiration_strategy(config: &CacheConfig, path: &Path) -> io::Result<ExpirationStrategy> {
        let bv = ByteView::open(path)?;
        let cache_entry = cache_entry_from_bytes(bv);
        Ok(super::expiration_strategy(config, &cache_entry))
    }

    #[test]
    fn test_expiration_strategy_positive() -> Result<()> {
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
        let (entry, _expiration) = cache.open_cachefile(&path)?.expect("No file found");
        assert_eq!(entry.unwrap().as_slice(), b"world");

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
                ..Default::default()
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
    async fn test_cache_error_write_negative() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);
        let error = CacheError::NotFound;
        error.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(current_pos, 0);

        let contents = fs::read(&path)?;
        assert_eq!(contents, b"");

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_error_write_negative_with_garbage() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);
        async_file.write_all(b"beep").await?;
        let error = CacheError::NotFound;
        error.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(current_pos, 0);

        let contents = fs::read(&path)?;
        assert_eq!(contents, b"");

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_error_write_malformed() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);

        let error_message = "unsupported object file format";
        let error = CacheError::Malformed(error_message.to_owned());
        error.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(
            current_pos as usize,
            CacheError::MALFORMED_MARKER.len() + error_message.len()
        );

        let contents = fs::read(&path)?;

        let mut expected: Vec<u8> = Vec::new();
        expected.extend(CacheError::MALFORMED_MARKER);
        expected.extend(error_message.as_bytes());

        assert_eq!(contents, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_error_write_malformed_truncates() -> Result<()> {
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
        let error = CacheError::Malformed(error_message.to_owned());
        error.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(
            current_pos as usize,
            CacheError::MALFORMED_MARKER.len() + error_message.len()
        );

        let contents = fs::read(&path)?;

        let mut expected: Vec<u8> = Vec::new();
        expected.extend(CacheError::MALFORMED_MARKER);
        expected.extend(error_message.as_bytes());

        assert_eq!(contents, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_error_write_cache_error() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("honk");

        // copying what compute does here instead of just using
        // tokio::fs::File::create() directly
        let sync_file = File::create(&path)?;
        let mut async_file = tokio::fs::File::from_std(sync_file);

        let error = CacheError::PermissionDenied("".into());
        error.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(
            current_pos as usize,
            CacheError::LEGACY_PERMISSION_DENIED_MARKER.len()
        );

        let contents = fs::read(&path)?;

        let mut expected: Vec<u8> = Vec::new();
        expected.extend(CacheError::LEGACY_PERMISSION_DENIED_MARKER);

        assert_eq!(contents, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_cache_error_write_cache_error_truncates() -> Result<()> {
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

        let error = CacheError::PermissionDenied("".into());
        error.write(&mut async_file).await?;

        // make sure write leaves the cursor at the end
        let current_pos = async_file.stream_position().await?;
        assert_eq!(
            current_pos as usize,
            CacheError::LEGACY_PERMISSION_DENIED_MARKER.len()
        );

        let contents = fs::read(&path)?;

        let mut expected: Vec<u8> = Vec::new();
        expected.extend(CacheError::LEGACY_PERMISSION_DENIED_MARKER);

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

    #[test]
    fn test_cache_entry() {
        fn read_cache_entry(bytes: &'static [u8]) -> CacheEntry<String> {
            cache_entry_from_bytes(ByteView::from_slice(bytes))
                .map(|bv| String::from_utf8_lossy(bv.as_slice()).into_owned())
        }

        let not_found = b"";

        assert_eq!(read_cache_entry(not_found), Err(CacheError::NotFound));

        let malformed = b"malformedDoesn't look like anything to me";

        assert_eq!(
            read_cache_entry(malformed),
            Err(CacheError::Malformed(
                "Doesn't look like anything to me".into()
            ))
        );

        let timeout = b"timeout4m33s";

        assert_eq!(
            read_cache_entry(timeout),
            Err(CacheError::Timeout(Duration::from_secs(273)))
        );

        let download_error = b"downloaderrorSomeone unplugged the internet";

        assert_eq!(
            read_cache_entry(download_error),
            Err(CacheError::DownloadError(
                "Someone unplugged the internet".into()
            ))
        );

        let permission_denied = b"permissiondeniedI'm sorry Dave, I'm afraid I can't do that";

        assert_eq!(
            read_cache_entry(permission_denied),
            Err(CacheError::PermissionDenied(
                "I'm sorry Dave, I'm afraid I can't do that".into()
            ))
        );

        let all_good = b"Not any of the error cases";

        assert_eq!(
            read_cache_entry(all_good),
            Ok("Not any of the error cases".into())
        );
    }
}
