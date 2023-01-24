use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;
use tokio::fs;

pub use super::cache_key::CacheKey;
use crate::cache::{Cache, CacheEntry, CacheError, ExpirationTime};
use crate::services::shared_cache::{CacheStoreReason, SharedCacheKey, SharedCacheRef};
use crate::utils::futures::CallOnDrop;

type InMemoryCache<T> = moka::future::Cache<CacheKey, CacheEntry<T>>;

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

    /// An in-memory Cache for some items which also does request-coalescing when requesting items.
    cache: InMemoryCache<T::Item>,

    /// A [`HashSet`] of currently running cache refreshes.
    refreshes: Arc<Mutex<HashSet<CacheKey>>>,

    /// A service used to communicate with the shared cache.
    shared_cache: SharedCacheRef,
}

// FIXME(swatinem): This is currently ~216 bytes that we copy around when spawning computations.
// The different cache actors have this behind an `Arc` already, maybe we should move that internally.
impl<T: CacheItemRequest> Clone for Cacher<T> {
    fn clone(&self) -> Self {
        // https://github.com/rust-lang/rust/issues/26925
        Cacher {
            config: self.config.clone(),
            cache: self.cache.clone(),
            refreshes: Arc::clone(&self.refreshes),
            shared_cache: Arc::clone(&self.shared_cache),
        }
    }
}

impl<T: CacheItemRequest> Cacher<T> {
    pub fn new(config: Cache, shared_cache: SharedCacheRef) -> Self {
        // TODO: eventually hook up configuration to this
        // The capacity(0) and ttl(0) make sure we are not (yet) reusing in-memory caches, except
        // for request coalescing right now.
        let cache = InMemoryCache::builder()
            .max_capacity(0)
            .time_to_live(Duration::ZERO)
            .build();
        Cacher {
            config,
            cache,
            refreshes: Default::default(),
            shared_cache,
        }
    }

    pub fn tempfile(&self) -> std::io::Result<NamedTempFile> {
        self.config.tempfile()
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

pub trait CacheItemRequest: 'static + Send + Sync + Clone {
    type Item: 'static + Send + Sync + Clone;

    /// The cache versioning scheme that is used for this type of request.
    ///
    /// Defaults to a scheme that does not use versioned cache files.
    const VERSIONS: CacheVersions = CacheVersions {
        current: 0,
        fallbacks: &[],
    };

    /// Invoked to compute an instance of this item and put it at the given location in the file
    /// system. This is used to populate the cache for a previously missing element.
    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry>;

    /// Determines whether this item should be loaded.
    ///
    /// If this returns `false` the cache will re-computed and be overwritten with the new
    /// result.
    fn should_load(&self, _data: &[u8]) -> bool {
        true
    }

    /// Loads an existing element from the cache.
    fn load(&self, data: ByteView<'static>, expiration: ExpirationTime) -> CacheEntry<Self::Item>;
}

impl<T: CacheItemRequest> Cacher<T> {
    /// Look up an item in the file system cache and load it if available.
    ///
    /// Returns `Err(NotFound)` if the cache item does not exist or needs to be re-computed.
    /// Otherwise returns another `CacheEntry`, which itself can be `NotFound`.
    fn lookup_local_cache(
        &self,
        request: &T,
        cache_dir: &Path,
        key: &CacheKey,
        version: u32,
    ) -> CacheEntry<CacheEntry<T::Item>> {
        let name = self.config.name();
        let item_path = key.cache_path(cache_dir, version);
        tracing::trace!("Trying {} cache at path {}", name, item_path.display());
        let _scope = Hub::current().push_scope();
        sentry::configure_scope(|scope| {
            scope.set_extra(
                &format!("cache.{name}.cache_path"),
                item_path.to_string_lossy().into(),
            );
        });
        let (entry, expiration) = self
            .config
            .open_cachefile(&item_path)?
            .ok_or(CacheError::NotFound)?;

        if let Ok(byteview) = &entry {
            if !request.should_load(byteview) {
                tracing::trace!("Discarding {} at path {}", name, item_path.display());
                metric!(counter(&format!("caches.{name}.file.discarded")) += 1);
                return Err(CacheError::NotFound);
            }

            // store things into the shared cache when:
            // - we have a positive cache
            // - that has the latest version (we don’t want to upload old versions)
            // - we refreshed the local cache time, so we also refresh the shared cache time.
            let needs_reupload = expiration.was_touched();
            if version == T::VERSIONS.current && needs_reupload {
                if let Some(shared_cache) = self.shared_cache.get() {
                    let shared_cache_key = SharedCacheKey {
                        name: self.config.name(),
                        version: T::VERSIONS.current,
                        local_key: key.clone(),
                    };
                    shared_cache.store(
                        shared_cache_key,
                        byteview.clone(),
                        CacheStoreReason::Refresh,
                    );
                }
            }
        }

        // This is also reported for "negative cache hits": When we cached
        // the 404 response from a server as empty file.
        metric!(counter(&format!("caches.{name}.file.hit")) += 1);
        if let Ok(byteview) = &entry {
            metric!(
                time_raw(&format!("caches.{name}.file.size")) = byteview.len() as u64,
                "hit" => "true"
            );
        }

        tracing::trace!("Loading {} at path {}", name, item_path.display());

        Ok(entry.and_then(|byteview| request.load(byteview, expiration)))
    }

    /// Compute an item.
    ///
    /// The item is computed using [`T::compute`](CacheItemRequest::compute), and saved in the cache
    /// if one is configured. The `is_refresh` flag is used only to tag computation metrics.
    ///
    /// This method does not take care of ensuring the computation only happens once even
    /// for concurrent requests, see the public [`Cacher::compute_memoized`] for this.
    async fn compute(&self, request: T, key: &CacheKey, is_refresh: bool) -> CacheEntry<T::Item> {
        let mut temp_file = self.tempfile()?;
        let shared_cache_key = SharedCacheKey {
            name: self.config.name(),
            version: T::VERSIONS.current,
            local_key: key.clone(),
        };

        let temp_fd = tokio::fs::File::from_std(temp_file.reopen()?);
        let shared_cache_hit = if let Some(shared_cache) = self.shared_cache.get() {
            shared_cache.fetch(&shared_cache_key, temp_fd).await
        } else {
            false
        };

        let mut entry = Err(CacheError::NotFound);
        if shared_cache_hit {
            // Waste an mmap call on a cold path, oh well.
            let bv = ByteView::map_file_ref(temp_file.as_file())?;
            if request.should_load(&bv) {
                entry = Ok(bv);
            } else {
                tracing::trace!("Discarding item from shared cache {}", key);
                metric!(counter("shared_cache.file.discarded") += 1);
            }
        }

        if entry.is_err() {
            match request.compute(&mut temp_file).await {
                Ok(()) => {
                    // Now we have written the data to the tempfile we can mmap it, persisting it later
                    // is fine as it does not move filesystem boundaries there.
                    let byte_view = ByteView::map_file_ref(temp_file.as_file())?;
                    entry = Ok(byte_view);
                }
                Err(err) => {
                    let mut temp_fd = tokio::fs::File::from_std(temp_file.reopen()?);
                    err.write(&mut temp_fd).await?;

                    entry = Err(err);
                }
            }
        }

        if let Some(cache_dir) = self.config.cache_dir() {
            // Cache is enabled, write it!
            let cache_path = key.cache_path(cache_dir, T::VERSIONS.current);

            sentry::configure_scope(|scope| {
                scope.set_extra(
                    &format!("cache.{}.cache_path", self.config.name()),
                    cache_path.to_string_lossy().into(),
                );
            });
            metric!(
                counter(&format!("caches.{}.file.write", self.config.name())) += 1,
                "status" => match &entry {
                    Ok(_) => "positive",
                    // TODO: should we create a `metrics_tag` method?
                    Err(CacheError::NotFound) => "negative",
                    Err(CacheError::Malformed(_)) => "malformed",
                    Err(_) => "cache-specific error",
                },
                "is_refresh" => &is_refresh.to_string(),
            );
            if let Ok(byte_view) = &entry {
                metric!(
                    time_raw(&format!("caches.{}.file.size", self.config.name())) = byte_view.len() as u64,
                    "hit" => "false",
                    "is_refresh" => &is_refresh.to_string(),
                );
            }

            tracing::trace!(
                "Creating {} at path {:?}",
                self.config.name(),
                cache_path.display()
            );

            persist_tempfile(temp_file, cache_path).await?;
        };

        // TODO: Not handling negative caches probably has a huge perf impact.  Need to
        // figure out negative caches.  Maybe put them in redis with a TTL?
        if !shared_cache_hit {
            if let Ok(byteview) = &entry {
                if let Some(shared_cache) = self.shared_cache.get() {
                    shared_cache.store(shared_cache_key, byteview.clone(), CacheStoreReason::New);
                }
            }
        }

        // we just created a fresh cache, so use the initial expiration times
        let expiration = ExpirationTime::for_fresh_status(&self.config, &entry);

        entry.and_then(|byteview| request.load(byteview, expiration))

        // TODO: log error:
        // sentry::configure_scope(|scope| {
        //     scope.set_extra("cache_key", self.get_cache_key().to_string().into());
        // });
        // sentry::capture_error(&*err);
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
    /// will return an `Err`. This err may be persisted in the cache for a time.
    pub async fn compute_memoized(&self, request: T, cache_key: CacheKey) -> CacheEntry<T::Item> {
        let name = self.config.name();

        let compute = Box::pin(async {
            // cache_path is None when caching is disabled.
            if let Some(cache_dir) = self.config.cache_dir() {
                let versions = std::iter::once(T::VERSIONS.current)
                    .chain(T::VERSIONS.fallbacks.iter().copied());

                for version in versions {
                    let item =
                        match self.lookup_local_cache(&request, cache_dir, &cache_key, version) {
                            Err(CacheError::NotFound) => continue,
                            Err(err) => return Err(err),
                            Ok(item) => item,
                        };

                    if version != T::VERSIONS.current {
                        // we have found an outdated cache that we will use right away,
                        // and we will kick off a recomputation for the `current` cache version
                        // in a deduplicated background task, which we will not await
                        metric!(
                            counter(&format!("caches.{name}.file.fallback")) += 1,
                            "version" => &version.to_string(),
                        );
                        self.spawn_refresh(cache_key.clone(), request);
                    }

                    return item;
                }
            }

            // A file was not found. If this spikes, it's possible that the filesystem cache
            // just got pruned.
            metric!(counter(&format!("caches.{name}.file.miss")) += 1);

            self.compute(request, &cache_key, false).await
        });

        self.cache.get_with_by_ref(&cache_key, compute).await
    }

    fn spawn_refresh(&self, cache_key: CacheKey, request: T) {
        let Some(cache_dir) = self.config.cache_dir() else { return };
        let name = self.config.name();

        let mut refreshes = self.refreshes.lock();
        if refreshes.contains(&cache_key) {
            return;
        }

        // We count down towards zero, and if we reach or surpass it, we will stop here.
        let max_lazy_refreshes = self.config.max_lazy_refreshes();
        if max_lazy_refreshes.fetch_sub(1, Ordering::Relaxed) <= 0 {
            max_lazy_refreshes.fetch_add(1, Ordering::Relaxed);

            metric!(counter("caches.lazy_limit_hit") += 1, "cache" => name.as_ref());
            return;
        }

        let done_token = {
            let key = cache_key.clone();
            let refreshes = Arc::clone(&self.refreshes);
            CallOnDrop::new(move || {
                max_lazy_refreshes.fetch_add(1, Ordering::Relaxed);
                refreshes.lock().remove(&key);
            })
        };
        refreshes.insert(cache_key.clone());
        drop(refreshes);

        tracing::trace!(
            "Spawning deduplicated {} computation for path {:?}",
            name,
            cache_key
                .cache_path(cache_dir, T::VERSIONS.current)
                .display()
        );

        let this = self.clone();
        let task = async move {
            let _done_token = done_token; // move into the future

            let span = sentry::configure_scope(|scope| scope.get_span());
            let ctx = sentry::TransactionContext::continue_from_span(
                "Lazy Cache Computation",
                "spawn_computation",
                span,
            );
            let transaction = sentry::start_transaction(ctx);
            sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));

            let result = this.compute(request, &cache_key, true).await;

            // refresh the memory cache with the newly refreshed result
            this.cache.insert(cache_key, result).await;

            transaction.finish();
        };
        tokio::spawn(task.bind_hub(Hub::new_from_top(Hub::current())));
    }
}

async fn persist_tempfile(
    mut temp_file: NamedTempFile,
    cache_path: PathBuf,
) -> std::io::Result<std::fs::File> {
    let parent = cache_path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "no parent directory to persist item",
        )
    })?;

    // The `cleanup` process could potentially remove the parent directories we are
    // operating in, so be defensive here and retry the fs operations.
    const MAX_RETRIES: usize = 2;
    let mut retries = 0;
    let file = loop {
        retries += 1;

        if let Err(e) = fs::create_dir_all(parent).await {
            sentry::with_scope(
                |scope| scope.set_extra("path", parent.display().to_string().into()),
                || tracing::error!("Failed to create cache directory: {:?}", e),
            );
            if retries > MAX_RETRIES {
                return Err(e);
            }
            continue;
        }

        match temp_file.persist(&cache_path) {
            Ok(file) => break file,
            Err(e) => {
                temp_file = e.file;
                let err = e.error;
                sentry::with_scope(
                    |scope| scope.set_extra("path", cache_path.display().to_string().into()),
                    || tracing::error!("Failed to create cache file: {:?}", err),
                );
                if retries > MAX_RETRIES {
                    return Err(err);
                }
                continue;
            }
        }
    };
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::cache::CacheName;
    use crate::config::{CacheConfig, CacheConfigs};
    use crate::test;

    use super::*;

    #[derive(Clone, Default)]
    struct TestCacheItem {
        computations: Arc<AtomicUsize>,
    }

    impl TestCacheItem {
        fn new() -> Self {
            Self {
                computations: Default::default(),
            }
        }
    }

    impl CacheItemRequest for TestCacheItem {
        type Item = String;

        const VERSIONS: CacheVersions = CacheVersions {
            current: 1,
            fallbacks: &[0],
        };

        fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
            self.computations.fetch_add(1, Ordering::SeqCst);

            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;

                std::fs::write(temp_file.path(), "some new cached contents")?;
                Ok(())
            })
        }

        fn load(
            &self,
            data: ByteView<'static>,
            _expiration: ExpirationTime,
        ) -> CacheEntry<Self::Item> {
            Ok(std::str::from_utf8(data.as_slice()).unwrap().to_owned())
        }
    }

    /// Tests that the size of the `compute_memoized` future does not grow out of bounds.
    /// See <https://github.com/moka-rs/moka/issues/212> for one of the main issues here.
    /// The size assertion will naturally change with compiler, dependency and code changes.
    #[tokio::test]
    async fn future_size() {
        test::setup();

        let cache = Cache::from_config(
            CacheName::Objects,
            None,
            None,
            CacheConfig::from(CacheConfigs::default().derived),
            Arc::new(AtomicIsize::new(1)),
        )
        .unwrap();
        let cacher = Cacher::new(cache, Default::default());

        let fut = cacher.compute_memoized(TestCacheItem::new("foo"));
        let size = dbg!(std::mem::size_of_val(&fut));
        assert!(size > 800 && size < 850);
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
            CacheName::Objects,
            Some(cache_dir),
            None,
            CacheConfig::from(CacheConfigs::default().derived),
            Arc::new(AtomicIsize::new(1)),
        )
        .unwrap();
        let cacher = Cacher::new(cache, Default::default());

        let request = TestCacheItem::new();
        let key = CacheKey {
            cache_key: "global/some_cache_key".into(),
        };

        let first_result = cacher.compute_memoized(request.clone(), key.clone()).await;
        assert_eq!(first_result.unwrap().as_str(), "some old cached contents");

        let second_result = cacher.compute_memoized(request.clone(), key.clone()).await;
        assert_eq!(second_result.unwrap().as_str(), "some old cached contents");

        tokio::time::sleep(Duration::from_millis(200)).await;

        let third_result = cacher.compute_memoized(request.clone(), key).await;
        assert_eq!(third_result.unwrap().as_str(), "some new cached contents");

        // we only want to have the actual computation be done a single time
        assert_eq!(request.computations.load(Ordering::SeqCst), 1);
    }

    /// Makes sure that a `NotFound` result does not fall back to older cache versions.
    #[tokio::test]
    async fn test_cache_fallback_notfound() {
        test::setup();

        let cache_dir = test::tempdir().path().join("test");
        std::fs::create_dir_all(cache_dir.join("global")).unwrap();
        std::fs::write(
            cache_dir.join("global/some_cache_key"),
            "some old cached contents",
        )
        .unwrap();
        std::fs::create_dir_all(cache_dir.join("1/global")).unwrap();
        std::fs::write(cache_dir.join("1/global/some_cache_key"), "").unwrap();

        let cache = Cache::from_config(
            CacheName::Objects,
            Some(cache_dir),
            None,
            CacheConfig::from(CacheConfigs::default().derived),
            Arc::new(AtomicIsize::new(1)),
        )
        .unwrap();
        let cacher = Cacher::new(cache, Default::default());

        let request = TestCacheItem::new();
        let key = CacheKey {
            cache_key: "global/some_cache_key".into(),
        };

        let first_result = cacher.compute_memoized(request.clone(), key).await;
        assert_eq!(first_result, Err(CacheError::NotFound));

        // no computation should be done
        assert_eq!(request.computations.load(Ordering::SeqCst), 0);
    }

    /// This test asserts that the bounded maximum number of recomputations is not exceeded.
    #[tokio::test]
    async fn test_lazy_computation_limit() {
        test::setup();

        let keys = &["global/1", "global/2", "global/3"];

        let cache_dir = test::tempdir().path().join("test");
        std::fs::create_dir_all(cache_dir.join("global")).unwrap();
        for key in keys {
            let path = cache_dir.join(key);
            std::fs::write(path, "some old cached contents").unwrap();
        }

        let cache = Cache::from_config(
            CacheName::Objects,
            Some(cache_dir),
            None,
            CacheConfig::from(CacheConfigs::default().derived),
            Arc::new(AtomicIsize::new(1)),
        )
        .unwrap();
        let cacher = Cacher::new(cache, Default::default());

        let request = TestCacheItem::new();

        for key in keys {
            let request = request.clone();
            let key = CacheKey {
                cache_key: String::from(*key),
            };

            let result = cacher.compute_memoized(request.clone(), key).await;
            assert_eq!(result.unwrap().as_str(), "some old cached contents");
        }

        tokio::time::sleep(Duration::from_millis(200)).await;

        // we want the actual computation to be done only one time, as that is the
        // maximum number of lazy computations.
        assert_eq!(request.computations.load(Ordering::SeqCst), 1);

        // double check that we actually get outdated contents for two of the requests.
        let mut num_outdated = 0;

        for key in keys {
            let request = request.clone();
            let key = CacheKey {
                cache_key: String::from(*key),
            };

            let result = cacher.compute_memoized(request.clone(), key).await;
            if result.unwrap().as_str() == "some old cached contents" {
                num_outdated += 1;
            }
        }

        assert_eq!(num_outdated, 2);
    }
}
