use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicIsize;
use std::time::{Duration, Instant, SystemTime};

use filetime::FileTime;
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::config::{CacheConfig, Config};
use crate::types::Scope;

use super::cache_error::cache_contents_from_bytes;
use super::{CacheContents, CacheEntry, CacheError, CacheName, Metadata};

/// The interval in which positive caches should be touched.
///
/// Positive "good" caches use a "time to idle" instead of "time to live" mode.
/// We thus need to regularly "touch" the files to signal that they are still in use.
/// This is being debounced to once every hour to not have to touch them on every single use.
const TOUCH_EVERY: Duration = Duration::from_secs(3600);

/// Extension for [`Metadata`] files.
pub(crate) const METADATA_EXTENSION: &str = "metadata";

/// Common cache configuration.
///
/// Many parts of Symbolicator use a cache to save having to re-download data or reprocess
/// downloaded data.  All caches behave similarly and their behaviour is determined by this
/// struct.
#[derive(Debug, Clone)]
pub struct Cache {
    /// Cache identifier used for metric names.
    pub(super) name: CacheName,

    /// Directory to use for storing cache items. Will be created if it does not exist.
    ///
    /// Leaving this as None will disable this cache.
    pub(super) cache_dir: Option<PathBuf>,

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

    /// The capacity (in bytes) of the in-memory cache.
    pub(super) in_memory_capacity: u64,
}

impl Cache {
    pub fn from_config(
        name: CacheName,
        config: &Config,
        cache_config: CacheConfig,
        max_lazy_refreshes: Arc<AtomicIsize>,
        in_memory_capacity: u64,
    ) -> io::Result<Self> {
        let tmp_dir = config.cache_dir("tmp");
        let cache_dir = config.cache_dir(name.as_ref());

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
            in_memory_capacity,
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

    /// Validate cache expiration of path.
    ///
    /// If cache should not be used, `Err(io::ErrorKind::NotFound)` is returned.
    /// If cache is usable, `Ok(x)` is returned with the opened [`ByteView`], and
    /// an [`ExpirationTime`] that indicates whether the file should be touched before using.
    pub(super) fn check_expiry(
        &self,
        path: &Path,
    ) -> io::Result<(CacheEntry<ByteView<'static>>, ExpirationTime)> {
        let fs_metadata = path.metadata()?;
        tracing::trace!("File `{}` length: {}", path.display(), fs_metadata.len());

        // Open the metadata file if possible
        let external_metadata = match self.read_metadata(path) {
            Ok(external_metadata) => external_metadata,
            Err(e) => {
                tracing::error!(
                    path = %path.display(),
                    error = &e as &dyn std::error::Error,
                    "Failed to parse cache metadata file",
                );
                let _ = std::fs::remove_file(metadata_path(path));
                None
            }
        };

        let (atime, ctime) = {
            // If the `ctime` from the external metadata is available, we use it. Otherwise
            // we use `mtime` to keep track of both "cache last used" and "cache created" depending on
            // whether the file is a negative cache item or not, because literally every other
            // filesystem attribute is unreliable.
            //
            // * creation time does not exist pre-Linux 4.11
            // * most filesystems are mounted with noatime
            let mtime = fs_metadata.modified()?;
            let ctime = external_metadata
                .as_ref()
                .map_or(mtime, |md| md.time_created);
            (mtime, ctime)
        };

        // Open the cache file itself
        let bv = ByteView::open(path)?;
        let contents = cache_contents_from_bytes(bv);

        // A cache entry is considered "public" if it came from a request with global scope.
        let is_public = external_metadata
            .as_ref()
            .is_some_and(|md| md.scope == Scope::Global);
        // States a cache item can be in:
        // * negative/empty: An empty file. Represents a failed download. ctime is used to indicate
        //   when the failed download happened (when the file was created)
        // * malformed: A file with the content `b"malformed"`. Represents a failed symcache
        //   conversion. ctime indicates when we attempted to convert.
        // * ok (don't really have a name): File has any other content, mtime is used to keep track
        //   of last use.
        let expiration_time = self.compute_expiration_time(&contents, atime, ctime, is_public)?;

        Ok((
            CacheEntry {
                metadata: external_metadata,
                contents,
            },
            expiration_time,
        ))
    }

    /// Computes an [`ExpirationTime`] for a given [`CacheContents`]
    /// which was last accessed at `atime` and originally written at `ctime`.
    ///
    /// The returned `ExpirationTime` determines when the cache contents should
    /// be touched to keep them alive or recomputed entirely.
    ///
    /// Negative cache entries for "public" files have a different (typically longer)
    /// lifetime than "private" (project-specific) ones. This is because public symbol
    /// sources are expected to change much more slowly.
    ///
    /// If the file is expired, this returns [`io::ErrorKind::NotFound`].
    fn compute_expiration_time<T>(
        &self,
        contents: &CacheContents<T>,
        atime: SystemTime,
        ctime: SystemTime,
        is_public: bool,
    ) -> io::Result<ExpirationTime> {
        let atime_elapsed = atime.elapsed().unwrap_or_default();
        let ctime_elapsed = ctime.elapsed().unwrap_or_default();

        match expiration_strategy(contents) {
            ExpirationStrategy::None => {
                let max_unused_for = self.cache_config.max_unused_for().unwrap_or(Duration::MAX);

                if atime_elapsed > max_unused_for {
                    return Err(io::ErrorKind::NotFound.into());
                }

                // we want to touch good caches once every `TOUCH_EVERY`
                let touch_in = TOUCH_EVERY.saturating_sub(atime_elapsed);
                Ok(ExpirationTime::TouchIn(touch_in))
            }
            ExpirationStrategy::Negative => {
                let retry_misses_after = if is_public {
                    self.cache_config
                        .retry_misses_after_public()
                        .unwrap_or(Duration::MAX)
                } else {
                    self.cache_config
                        .retry_misses_after()
                        .unwrap_or(Duration::MAX)
                };

                let expires_in = retry_misses_after.saturating_sub(ctime_elapsed);

                if expires_in == Duration::ZERO {
                    return Err(io::ErrorKind::NotFound.into());
                }

                Ok(ExpirationTime::RefreshIn(expires_in))
            }
            ExpirationStrategy::Malformed => {
                let retry_malformed_after = self
                    .cache_config
                    .retry_malformed_after()
                    .unwrap_or(Duration::MAX);

                let expires_in = retry_malformed_after.saturating_sub(ctime_elapsed);

                // Immediately expire malformed items that have been created before this process started.
                // See docstring of MALFORMED_MARKER
                if ctime < self.start_time || expires_in == Duration::ZERO {
                    tracing::trace!("Created at is older than start time");
                    return Err(io::ErrorKind::NotFound.into());
                }

                Ok(ExpirationTime::RefreshIn(expires_in))
            }
        }
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

    /// Reads [`Metadata`] for the cache entry in `path` from the file at `path.metadata`.
    ///
    /// If the metadata file doesn't exist, this returns `Ok(None)`.
    pub fn read_metadata(&self, path: &Path) -> io::Result<Option<Metadata>> {
        let md_path = metadata_path(path);

        catch_not_found(|| {
            let file = File::open(&md_path)?;
            let reader = BufReader::new(file);
            serde_json::from_reader(reader)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
#[derive(Debug, Clone)]
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
    /// Gives the [`ExpirationTime`] for a freshly created cache with the given [`CacheContents`].
    pub fn for_fresh_status<T>(cache: &Cache, entry: &CacheContents<T>) -> Self {
        let config = &cache.cache_config;
        let strategy = expiration_strategy(entry);
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

    /// Gives the [`Instant`] at which the item expires.
    pub fn as_instant(&self) -> Instant {
        let duration = match self {
            ExpirationTime::RefreshIn(d) => d,
            ExpirationTime::TouchIn(d) => d,
        };
        Instant::now() + *duration
    }
}

/// Checks the cache contents in `buf` and returns the cleanup strategy that should be used
/// for the item.
pub(super) fn expiration_strategy<T>(status: &CacheContents<T>) -> ExpirationStrategy {
    match status {
        Ok(_) => ExpirationStrategy::None,
        Err(CacheError::Malformed(_)) => ExpirationStrategy::Malformed,
        // All other errors should be treated as "negative" in terms of the expiration.
        Err(_) => ExpirationStrategy::Negative,
    }
}

pub(super) fn catch_not_found<F, R>(f: F) -> io::Result<Option<R>>
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

/// Returns the corresponding metadata file path
/// for a cache file.
pub(crate) fn metadata_path(path: impl AsRef<Path>) -> PathBuf {
    let mut out = path.as_ref().to_path_buf();
    out.set_extension(METADATA_EXTENSION);
    out
}

/// Writes metadata for the cache entry at the given path.
pub(crate) fn write_metadata(path: impl AsRef<Path>, metadata: &Metadata) -> io::Result<()> {
    let md_path = metadata_path(path);
    let mut md_file = File::create(md_path)?;
    serde_json::to_writer(&mut md_file, metadata).map_err(io::Error::other)
}
