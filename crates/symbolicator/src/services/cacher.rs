use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::io::Error;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::channel::oneshot;
use futures::future::{BoxFuture, FutureExt, Shared, TryFutureExt};
use parking_lot::Mutex;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::cache::{Cache, CacheStatus};
use crate::services::shared_cache::{SharedCacheKey, SharedCacheService};
use crate::types::Scope;
use crate::utils::futures::CallOnDrop;

type ComputationResult<T, E> = Result<Arc<T>, Arc<E>>;
// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<ComputationResult<T, E>>>;

type CacheResultFuture<T, E> = BoxFuture<'static, ComputationResult<T, E>>;

type ComputationMap<T, E> = Arc<Mutex<BTreeMap<CacheKey, ComputationChannel<T, E>>>>;

/// Manages a filesystem cache of any kind of data that can be serialized into bytes and read from
/// it:
///
/// - Object files
/// - Symcaches
/// - CFI caches
///
/// Transparently performs cache lookups, downloads and cache stores via the [`CacheItemRequest`]
/// trait and associated types.
///
/// Internally deduplicates concurrent cache lookups (in-memory).
#[derive(Debug)]
pub struct Cacher<T: CacheItemRequest> {
    config: Cache,

    /// Used for deduplicating cache lookups.
    current_computations: ComputationMap<T::Item, T::Error>,

    /// A service used to communicate with the shared cache.
    shared_cache_service: Arc<SharedCacheService>,
}

impl<T: CacheItemRequest> Clone for Cacher<T> {
    fn clone(&self) -> Self {
        // https://github.com/rust-lang/rust/issues/26925
        Cacher {
            config: self.config.clone(),
            current_computations: self.current_computations.clone(),
            shared_cache_service: Arc::clone(&self.shared_cache_service),
        }
    }
}

impl<T: CacheItemRequest> Cacher<T> {
    pub fn new(config: Cache, shared_cache_service: Arc<SharedCacheService>) -> Self {
        Cacher {
            config,
            shared_cache_service,
            current_computations: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn tempfile(&self) -> io::Result<NamedTempFile> {
        self.config.tempfile()
    }
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct CacheKey {
    pub cache_key: String,
    pub scope: Scope,
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (scope {})", self.cache_key, self.scope)
    }
}

impl CacheKey {
    /// Returns the relative path inside the cache for this cache key.
    pub fn relative_path(&self) -> PathBuf {
        let mut path = PathBuf::new();
        path.push(safe_path_segment(self.scope.as_ref()));
        path.push(safe_path_segment(&self.cache_key));
        path
    }

    /// Returns the full cache path for this key inside the provided cache.
    ///
    /// If caching is disabled returns `None`.
    pub fn cache_path(&self, version: u32, cache: &Cache) -> Option<PathBuf> {
        let mut path = cache.cache_dir()?.to_path_buf();
        if version != 0 {
            path.push(version.to_string());
        }
        path.push(self.relative_path());
        Some(path)
    }
}

/// Cache Version Configuration used during cache lookup and generation.
///
/// The `current` version is tried first, and written during cache generation.
/// The `fallback` versions are tried next, in first to last order. They are used only for cache
/// lookups, but never for writing.
///
/// The version `0` is special in the sense that it is not used as part of the resulting cache
/// file path, and generates the same paths as "legacy" unversioned cache files.
#[derive(Clone, Debug)]
pub struct CacheVersions {
    /// The current cache version that is being looked up, and used for writing
    pub current: u32,
    /// A list of fallback cache versions that are being tried on lookup,
    /// in descending order of priority.
    pub fallbacks: &'static [u32],
}

/// Path to a temporary or cached file.
///
/// This path can either point to a named temporary file, or a permanently cached file. If the file
/// is a named temporary, it will be removed once this path instance is dropped. If the file is
/// permanently cached, dropping this path does not remove it.
///
/// This path implements `Deref<Path>`, which means all non-mutating methods can be called on path
/// directly.
#[derive(Debug)]
pub enum CachePath {
    /// A named temporary file that will be deleted.
    Temp(tempfile::TempPath),
    /// A permanently cached file.
    Cached(PathBuf),
}

impl CachePath {
    /// Creates an empty cache path.
    pub fn new() -> Self {
        Self::Cached(PathBuf::new())
    }
}

impl std::ops::Deref for CachePath {
    type Target = Path;

    #[inline]
    fn deref(&self) -> &Self::Target {
        match *self {
            Self::Temp(ref temp) => temp,
            Self::Cached(ref buf) => buf,
        }
    }
}

impl AsRef<Path> for CachePath {
    #[inline]
    fn as_ref(&self) -> &Path {
        match *self {
            Self::Temp(ref temp) => temp,
            Self::Cached(ref buf) => buf,
        }
    }
}

fn safe_path_segment(s: &str) -> String {
    s.replace(".", "_") // protect against ".."
        .replace("/", "_") // protect against absolute paths
        .replace(":", "_") // not a threat on POSIX filesystems, but confuses OS X Finder
}

pub trait CacheItemRequest: 'static + Send + Sync + Clone {
    type Item: 'static + Send + Sync;

    // XXX: Probably should have our own concrete error type for cacheactor instead of forcing our
    // ioerrors into other errors
    type Error: 'static + From<io::Error> + Send + Sync;

    /// The cache versioning scheme that is used for this type of request.
    ///
    /// Defaults to a scheme that does not use versioned cache files.
    const VERSIONS: CacheVersions = CacheVersions {
        current: 0,
        fallbacks: &[],
    };

    /// Returns the key by which this item is cached.
    fn get_cache_key(&self) -> CacheKey;

    /// Invoked to compute an instance of this item and put it at the given location in the file
    /// system. This is used to populate the cache for a previously missing element.
    fn compute(&self, path: &Path) -> BoxFuture<'static, Result<CacheStatus, Self::Error>>;

    /// Determines whether this item should be loaded.
    ///
    /// If this returns `false` the cache will re-computed and be overwritten with the new
    /// result.
    fn should_load(&self, _data: &[u8]) -> bool {
        true
    }

    /// Loads an existing element from the cache.
    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        path: CachePath,
    ) -> Self::Item;
}

impl<T: CacheItemRequest> Cacher<T> {
    /// Look up an item in the file system cache and load it if available.
    ///
    /// Returns `Ok(None)` if the cache item needs to be re-computed, otherwise reads the
    /// cached data from disk and returns the cached item as returned by
    /// [`CacheItemRequest::load`].
    ///
    /// # Errors
    ///
    /// If there is an I/O error reading the cache [`CacheItemRequest::Error`] is returned.
    fn lookup_local_cache(
        &self,
        request: &T,
        key: &CacheKey,
        path: &Path,
    ) -> Result<Option<T::Item>, T::Error> {
        let name = self.config.name();
        log::trace!("Trying {} cache at path {:?}", name, path);
        sentry::with_scope(
            |scope| {
                scope.set_extra(
                    &format!("cache.{}.cache_path", name),
                    format!("{:?}", path).into(),
                );
            },
            || {
                let byteview = match self.config.open_cachefile(path)? {
                    Some(x) => x,
                    None => return Ok(None),
                };

                let status = CacheStatus::from_content(&byteview);
                if status == CacheStatus::Positive && !request.should_load(&byteview) {
                    log::trace!("Discarding {} at path {:?}", name, path);
                    metric!(counter(&format!("caches.{}.file.discarded", name)) += 1);
                    return Ok(None);
                }

                // This is also reported for "negative cache hits": When we cached the 404 response from a
                // server as empty file.
                metric!(counter(&format!("caches.{}.file.hit", name)) += 1);
                metric!(
                    time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
                    "hit" => "true"
                );

                let path = path.to_path_buf();
                log::trace!("Loading {} at path {:?}", name, path);
                let item =
                    request.load(key.scope.clone(), status, byteview, CachePath::Cached(path));
                Ok(Some(item))
            },
        )
    }

    /// Compute an item.
    ///
    /// The item is computed using [`T::compute`](CacheItemRequest::compute), and saved in the cache
    /// if one is configured. The `is_refresh` flag is used only to tag computation metrics.
    ///
    /// This method does not take care of ensuring the computation only happens once even
    /// for concurrent requests, see the public [`Cacher::compute_memoized`] for this.
    async fn compute(
        self,
        request: T,
        key: CacheKey,
        is_refresh: bool,
    ) -> Result<T::Item, T::Error> {
        // We do another cache lookup here. `compute_memoized` has a fast-path that does a cache
        // lookup without going through the deduplication/channel creation logic. This creates a
        // small opportunity of invoking compute another time after a fresh cache has just been
        // computed. To avoid duplicated work in that case, we will check the cache here again.
        let cache_path = key.cache_path(T::VERSIONS.current, &self.config);
        if let Some(ref path) = cache_path {
            if let Some(item) = self.lookup_local_cache(&request, &key, path)? {
                return Ok(item);
            }
        }

        let temp_file = self.tempfile()?;

        let scope = key.scope.clone();
        let shared_cache_key = SharedCacheKey {
            name: self.config.name(),
            version: T::VERSIONS.current,
            local_key: key,
        };
        let shared_cache_hit = match self
            .shared_cache_service
            .fetch(&shared_cache_key, temp_file.path())
            .await
        {
            Some(_) => {
                metric!(counter(&format!("shared_cache.{}", self.config.name())) += 1, "hit" => "true");
                true
            }
            None => {
                metric!(counter(&format!("shared_cache.{}", self.config.name())) += 1, "hit" => "false");
                false
            }
        };

        let (status, byte_view) = if shared_cache_hit {
            let byte_view = ByteView::open(temp_file.path())?;
            (CacheStatus::from_content(&byte_view), byte_view)
        } else {
            let status = request.compute(temp_file.path()).await?;
            let byte_view = ByteView::open(temp_file.path())?;
            (status, byte_view)
        };

        let path = match cache_path {
            Some(ref cache_path) => {
                metric!(
                    counter(&format!("caches.{}.file.write", self.config.name())) += 1,
                    "status" => status.as_ref(),
                    "is_refresh" => &is_refresh.to_string(),
                );
                metric!(
                    time_raw(&format!("caches.{}.file.size", self.config.name())) = byte_view.len() as u64,
                    "hit" => "false",
                    "is_refresh" => &is_refresh.to_string(),
                );
                sentry::configure_scope(|scope| {
                    scope.set_extra(
                        &format!("cache.{}.cache_path", self.config.name()),
                        cache_path.to_string_lossy().into(),
                    );
                });
                log::trace!("Creating {} at path {:?}", self.config.name(), cache_path);

                status.persist_item(cache_path, temp_file)?;
                CachePath::Cached(cache_path.to_path_buf())
            }
            None => CachePath::Temp(temp_file.into_temp_path()),
        };

        // TODO: Not handling negative caches probably has a huge perf impact.  Need to
        // figure out negative caches.  Maybe also put them in the GCS bucket but make
        // them expire?
        if !shared_cache_hit && status == CacheStatus::Positive {
            self.shared_cache_service
                .store(shared_cache_key, byte_view.clone())
                .await;
            // TODO: maybe push the metrics to the SharedCacheService?  It would need to
            // know the cache for which this is being stored though.  Maybe that could
            // be done by changing the Path arg into a SharedCacheKey which has the
            // CacheKey, name and version.
            metric!(counter(&format!("shared_caches.{}.file.write", self.config.name())) += 1);
        }

        Ok(request.load(scope, status, byte_view, path))
    }

    /// Creates a shareable channel that computes an item.
    ///
    /// In case the `is_refresh` flag is set, the computation request will count towards the configured
    /// `max_lazy_refreshes`, and will return immediately with an error if the threshold was reached.
    fn create_channel<F>(
        &self,
        key: CacheKey,
        computation: F,
        is_refresh: bool,
    ) -> ComputationChannel<T::Item, T::Error>
    where
        F: std::future::Future<Output = Result<T::Item, T::Error>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let max_lazy_refreshes = self.config.max_lazy_refreshes();

        // we count down towards zero, and if we reach or surpass it, we will short circuit here.
        if is_refresh && max_lazy_refreshes.fetch_sub(1, Ordering::Relaxed) <= 0 {
            max_lazy_refreshes.fetch_add(1, Ordering::Relaxed);

            let result = Err(Arc::new(
                Error::new(
                    ErrorKind::Other,
                    "maximum number of lazy recomputations reached; aborting cache computation",
                )
                .into(),
            ));
            sender.send(result).ok();
            return receiver.shared();
        }

        let current_computations = self.current_computations.clone();
        let remove_computation_token = CallOnDrop::new(move || {
            if is_refresh {
                max_lazy_refreshes.fetch_add(1, Ordering::Relaxed);
            }
            current_computations.lock().remove(&key);
        });

        // Run the computation and wrap the result in Arcs to make them clonable.
        let channel = async move {
            let result = match computation.await {
                Ok(ok) => Ok(Arc::new(ok)),
                Err(err) => Err(Arc::new(err)),
            };
            // Drop the token first to evict from the map.  This ensures that callers either
            // get a channel that will receive data, or they create a new channel.
            drop(remove_computation_token);
            sender.send(result).ok();
        }
        .bind_hub(Hub::new_from_top(Hub::current()));

        // TODO: This spawns into the current_thread runtime of the caller. Consider more explicit
        // resource allocation here to separate CPU intensive work from I/O work.
        tokio::spawn(channel);

        receiver.shared()
    }

    /// Spawns the computation as a separate task.
    ///
    /// This does deduplication, by keeping track of the running computations based on their [`CacheKey`].
    ///
    /// NOTE: This function itself is *not* `async`, because it should eagerly spawn the computation
    /// on an executor, even if you don’t explicitly `await` its results.
    fn spawn_computation(
        &self,
        request: T,
        is_refresh: bool,
    ) -> CacheResultFuture<T::Item, T::Error> {
        let key = request.get_cache_key();
        let name = self.config.name();

        let channel = {
            let mut current_computations = self.current_computations.lock();
            if let Some(channel) = current_computations.get(&key) {
                // A concurrent cache lookup was deduplicated.
                metric!(counter(&format!("caches.{}.channel.hit", name)) += 1);
                channel.clone()
            } else {
                // A concurrent cache lookup is considered new. This does not imply a cache miss.
                metric!(counter(&format!("caches.{}.channel.miss", name)) += 1);
                let computation = self.clone().compute(request, key.clone(), is_refresh);
                let channel = self.create_channel(key.clone(), computation, is_refresh);
                let evicted = current_computations.insert(key, channel.clone());
                debug_assert!(evicted.is_none());
                channel
            }
        };

        let future = channel.unwrap_or_else(move |_cancelled_error| {
            let message = format!("{} computation channel dropped", name);
            Err(Arc::new(
                io::Error::new(io::ErrorKind::Interrupted, message).into(),
            ))
        });

        Box::pin(future)
    }

    /// Computes an item by loading from or populating the cache.
    ///
    /// The actual computation is deduplicated between concurrent requests. Finally, the result is
    /// inserted into the cache and all subsequent calls fetch from the cache.
    ///
    /// The computation itself is done by [`T::compute`](CacheItemRequest::compute), but only if it
    /// was not already in the cache.
    ///
    /// # Errors
    ///
    /// Cache computation can fail, in which case [`T::compute`](CacheItemRequest::compute)
    /// will return an error of type [`T::Error`](CacheItemRequest::Error).  When this
    /// occurs the error result is returned, **however** in this case nothing is written
    /// into the cache and the next call to the same cache item will attempt to re-compute
    /// the cache.
    pub async fn compute_memoized(&self, request: T) -> Result<Arc<T::Item>, Arc<T::Error>> {
        let name = self.config.name();
        let key = request.get_cache_key();

        // cache_path is None when caching is disabled.
        let cache_path = key.cache_path(T::VERSIONS.current, &self.config);
        if let Some(ref path) = cache_path {
            if let Some(item) = self.lookup_local_cache(&request, &key, path)? {
                return Ok(Arc::new(item));
            }

            // try fallback cache paths next
            for (version, fallback_path) in T::VERSIONS.fallbacks.iter().flat_map(|version| {
                key.cache_path(*version, &self.config)
                    .map(|path| (*version, path))
            }) {
                if let Ok(Some(item)) = self.lookup_local_cache(&request, &key, &fallback_path) {
                    // we have found an outdated cache that we will use right away,
                    // and we will kick off a recomputation for the `current` cache version
                    // in a deduplicated background task, which we will not await
                    log::trace!(
                        "Spawning deduplicated {} computation for path {:?}",
                        name,
                        path
                    );
                    metric!(
                        counter(&format!("caches.{}.file.fallback", name)) += 1,
                        "version" => &version.to_string(),
                    );
                    let _not_awaiting_future = self.spawn_computation(request, true);

                    return Ok(Arc::new(item));
                }
            }
        }

        // A file was not found. If this spikes, it's possible that the filesystem cache
        // just got pruned.
        metric!(counter(&format!("caches.{}.file.miss", name)) += 1);

        Ok(self.spawn_computation(request, false).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::config::{CacheConfig, CacheConfigs};
    use crate::test;

    use super::*;

    #[derive(Clone, Default)]
    struct TestCacheItem {
        computations: Arc<AtomicUsize>,
        key: &'static str,
    }

    impl TestCacheItem {
        fn new(key: &'static str) -> Self {
            Self {
                computations: Default::default(),
                key,
            }
        }
    }

    impl CacheItemRequest for TestCacheItem {
        type Item = String;

        type Error = std::io::Error;

        const VERSIONS: CacheVersions = CacheVersions {
            current: 1,
            fallbacks: &[0],
        };

        fn get_cache_key(&self) -> CacheKey {
            CacheKey {
                cache_key: String::from(self.key),
                scope: Scope::Global,
            }
        }

        fn compute(&self, path: &Path) -> BoxFuture<'static, Result<CacheStatus, Self::Error>> {
            self.computations.fetch_add(1, Ordering::SeqCst);

            let path = path.to_owned();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;

                std::fs::write(path, "some new cached contents")?;
                Ok(CacheStatus::Positive)
            })
        }

        fn load(
            &self,
            _scope: Scope,
            _status: CacheStatus,
            data: ByteView<'static>,
            _path: CachePath,
        ) -> Self::Item {
            std::str::from_utf8(data.as_slice()).unwrap().to_owned()
        }
    }

    /// This test asserts that the cache is served from outdated cache files, and that a computation
    /// is being kicked off (and deduplicated) in the background
    #[tokio::test]
    async fn test_cache_fallback() {
        test::setup();

        let cache_dir = test::tempdir().path().join("test");
        std::fs::create_dir_all(cache_dir.join("global")).unwrap();
        std::fs::write(
            cache_dir.join("global/some_cache_key"),
            "some old cached contents",
        )
        .unwrap();

        let cache = Cache::from_config(
            "test",
            Some(cache_dir),
            None,
            CacheConfig::from(CacheConfigs::default().derived),
            Arc::new(AtomicIsize::new(1)),
        )
        .unwrap();
        let cacher = Cacher::new(cache);

        let request = TestCacheItem::new("some_cache_key");

        let first_result = cacher.compute_memoized(request.clone()).await;
        assert_eq!(first_result.unwrap().as_str(), "some old cached contents");

        let second_result = cacher.compute_memoized(request.clone()).await;
        assert_eq!(second_result.unwrap().as_str(), "some old cached contents");

        tokio::time::sleep(Duration::from_millis(200)).await;

        let third_result = cacher.compute_memoized(request.clone()).await;
        assert_eq!(third_result.unwrap().as_str(), "some new cached contents");

        // we only want to have the actual computation be done a single time
        assert_eq!(request.computations.load(Ordering::SeqCst), 1);
    }

    /// This test asserts that the bounded maximum number of recomputations is not exceeded.
    #[tokio::test]
    async fn test_lazy_computation_limit() {
        test::setup();

        let keys = &["1", "2", "3"];

        let cache_dir = test::tempdir().path().join("test");
        std::fs::create_dir_all(cache_dir.join("global")).unwrap();
        for key in keys {
            let mut path = cache_dir.join("global");
            path.push(key);
            std::fs::write(path, "some old cached contents").unwrap();
        }

        let cache = Cache::from_config(
            "test",
            Some(cache_dir),
            None,
            CacheConfig::from(CacheConfigs::default().derived),
            Arc::new(AtomicIsize::new(1)),
        )
        .unwrap();
        let cacher = Cacher::new(cache);

        let request = TestCacheItem::new("0");

        for key in keys {
            let mut request = request.clone();
            request.key = key;

            let result = cacher.compute_memoized(request.clone()).await;
            assert_eq!(result.unwrap().as_str(), "some old cached contents");
        }

        tokio::time::sleep(Duration::from_millis(200)).await;

        // we want the actual computation to be done only one time, as that is the
        // maximum number of lazy computations.
        assert_eq!(request.computations.load(Ordering::SeqCst), 1);

        // double check that we actually get outdated contents for two of the requests.
        let mut num_outdated = 0;

        for key in keys {
            let mut request = request.clone();
            request.key = key;

            let result = cacher.compute_memoized(request.clone()).await;
            if result.unwrap().as_str() == "some old cached contents" {
                num_outdated += 1;
            }
        }

        assert_eq!(num_outdated, 2);
    }
}
