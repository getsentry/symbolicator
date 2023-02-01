use std::collections::BTreeMap;
use std::io::{self, Error, ErrorKind};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::channel::oneshot;
use futures::future::{BoxFuture, Shared};
use futures::{FutureExt, TryFutureExt};
use parking_lot::Mutex;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;
use tokio::fs;

use super::shared_cache::{CacheStoreReason, SharedCacheKey, SharedCacheRef};
use crate::utils::futures::CallOnDrop;

use super::{Cache, CacheEntry, CacheError, CacheKey, ExpirationTime};

type ComputationChannel<T> = Shared<oneshot::Receiver<CacheEntry<T>>>;
type ComputationMap<T> = Arc<Mutex<BTreeMap<CacheKey, ComputationChannel<T>>>>;

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
    current_computations: ComputationMap<T::Item>,

    /// A service used to communicate with the shared cache.
    shared_cache: SharedCacheRef,
}

impl<T: CacheItemRequest> Clone for Cacher<T> {
    fn clone(&self) -> Self {
        // https://github.com/rust-lang/rust/issues/26925
        Cacher {
            config: self.config.clone(),
            current_computations: self.current_computations.clone(),
            shared_cache: Arc::clone(&self.shared_cache),
        }
    }
}

impl<T: CacheItemRequest> Cacher<T> {
    pub fn new(config: Cache, shared_cache: SharedCacheRef) -> Self {
        Cacher {
            config,
            shared_cache,
            current_computations: Arc::new(Mutex::new(BTreeMap::new())),
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
        key: &CacheKey,
        version: u32,
    ) -> CacheEntry<CacheEntry<T::Item>> {
        let Some(cache_dir) = self.config.cache_dir() else {
            return Err(CacheError::NotFound);
        };
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
                metric!(counter("caches.file.discarded") += 1, "cache" => name.as_ref());
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
        metric!(counter("caches.file.hit") += 1, "cache" => name.as_ref());
        if let Ok(byteview) = &entry {
            metric!(
                time_raw("caches.file.size") = byteview.len() as u64,
                "hit" => "true",
                "cache" => name.as_ref(),
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
    async fn compute(self, request: T, key: CacheKey, is_refresh: bool) -> CacheEntry<T::Item> {
        // We do another cache lookup here. `compute_memoized` has a fast-path that does a cache
        // lookup without going through the deduplication/channel creation logic. This creates a
        // small opportunity of invoking compute another time after a fresh cache has just been
        // computed. To avoid duplicated work in that case, we will check the cache here again.
        if let Ok(item) = self.lookup_local_cache(&request, &key, T::VERSIONS.current) {
            return item;
        }

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
            let name = self.config.name();
            // Cache is enabled, write it!
            let cache_path = key.cache_path(cache_dir, T::VERSIONS.current);

            sentry::configure_scope(|scope| {
                scope.set_extra(
                    &format!("cache.{}.cache_path", self.config.name()),
                    cache_path.to_string_lossy().into(),
                );
            });
            metric!(
                counter("caches.file.write") += 1,
                "status" => match &entry {
                    Ok(_) => "positive",
                    // TODO: should we create a `metrics_tag` method?
                    Err(CacheError::NotFound) => "negative",
                    Err(CacheError::Malformed(_)) => "malformed",
                    Err(_) => "cache-specific error",
                },
                "is_refresh" => &is_refresh.to_string(),
                "cache" => name.as_ref(),
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

    /// Creates a shareable channel that computes an item.
    ///
    /// In case the `is_refresh` flag is set, the computation request will count towards the configured
    /// `max_lazy_refreshes`, and will return immediately with an error if the threshold was reached.
    fn create_channel<F>(
        &self,
        key: CacheKey,
        computation: F,
        is_refresh: bool,
    ) -> ComputationChannel<T::Item>
    where
        F: std::future::Future<Output = CacheEntry<T::Item>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();

        let max_lazy_refreshes = self.config.max_lazy_refreshes();
        let current_computations = self.current_computations.clone();
        let remove_computation_token = CallOnDrop::new(move || {
            if is_refresh {
                max_lazy_refreshes.fetch_add(1, Ordering::Relaxed);
            }
            current_computations.lock().remove(&key);
        });

        // Run the computation and wrap the result in Arcs to make them clonable.
        let channel = async move {
            // only start an independent transaction if this is a "background" task,
            // otherwise it will not "outlive" its parent span, so attach it to the parent transaction.
            let transaction = if is_refresh {
                let span = sentry::configure_scope(|scope| scope.get_span());
                let ctx = sentry::TransactionContext::continue_from_span(
                    "Lazy Cache Computation",
                    "spawn_computation",
                    span,
                );
                let transaction = sentry::start_transaction(ctx);
                sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));
                Some(transaction)
            } else {
                None
            };
            let result = computation.await;
            // Drop the token first to evict from the map.  This ensures that callers either
            // get a channel that will receive data, or they create a new channel.
            drop(remove_computation_token);
            if let Some(transaction) = transaction {
                transaction.finish();
            }
            sender.send(result).ok();
        }
        .bind_hub(Hub::new_from_top(Hub::current()));

        // These computations are spawned on the current runtime, which in all cases is the CPU-pool.
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
        cache_key: CacheKey,
        is_refresh: bool,
    ) -> BoxFuture<'static, CacheEntry<T::Item>> {
        let name = self.config.name();

        let channel = {
            let mut current_computations = self.current_computations.lock();
            if let Some(channel) = current_computations.get(&cache_key) {
                // A concurrent cache lookup was deduplicated.
                metric!(counter("caches.channel.hit") += 1, "cache" => name.as_ref());
                channel.clone()
            } else {
                // A concurrent cache lookup is considered new. This does not imply a cache miss.
                metric!(counter("caches.channel.miss") += 1, "cache" => name.as_ref());

                // We count down towards zero, and if we reach or surpass it, we will short circuit here.
                // Doing the short-circuiting here means we don't create a channel at all, and don't
                // put it into `current_computations`.
                let max_lazy_refreshes = self.config.max_lazy_refreshes();
                if is_refresh && max_lazy_refreshes.fetch_sub(1, Ordering::Relaxed) <= 0 {
                    max_lazy_refreshes.fetch_add(1, Ordering::Relaxed);

                    metric!(counter("caches.lazy_limit_hit") += 1, "cache" => name.as_ref());
                    // This error is purely here to satisfy the return type, it should not show
                    // up anywhere, as lazy computation will not unwrap the error.
                    let result = Err(Error::new(
                        ErrorKind::Other,
                        "maximum number of lazy recomputations reached; aborting cache computation",
                    )
                    .into());
                    return Box::pin(async { result });
                }

                let computation = self.clone().compute(request, cache_key.clone(), is_refresh);
                let channel = self.create_channel(cache_key.clone(), computation, is_refresh);
                let evicted = current_computations.insert(cache_key, channel.clone());
                debug_assert!(evicted.is_none());
                channel
            }
        };

        let future = channel.unwrap_or_else(move |_cancelled_error| {
            let message = format!("{name} computation channel dropped");
            Err(std::io::Error::new(std::io::ErrorKind::Interrupted, message).into())
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
    /// will return an `Err`. This err may be persisted in the cache for a time.
    pub async fn compute_memoized(&self, request: T, cache_key: CacheKey) -> CacheEntry<T::Item> {
        let name = self.config.name();

        // cache_path is None when caching is disabled.
        if let Some(cache_dir) = self.config.cache_dir() {
            let versions =
                std::iter::once(T::VERSIONS.current).chain(T::VERSIONS.fallbacks.iter().copied());

            for version in versions {
                let item = match self.lookup_local_cache(&request, &cache_key, version) {
                    Err(CacheError::NotFound) => continue,
                    Err(err) => return Err(err),
                    Ok(item) => item,
                };

                if version != T::VERSIONS.current {
                    // we have found an outdated cache that we will use right away,
                    // and we will kick off a recomputation for the `current` cache version
                    // in a deduplicated background task, which we will not await
                    tracing::trace!(
                        "Spawning deduplicated {} computation for path {:?}",
                        name,
                        cache_key
                            .cache_path(cache_dir, T::VERSIONS.current)
                            .display()
                    );
                    metric!(
                        counter("caches.file.fallback") += 1,
                        "version" => &version.to_string(),
                        "cache" => name.as_ref(),
                    );
                    let _not_awaiting_future = self.spawn_computation(request, cache_key, true);
                }

                return item;
            }
        }

        // A file was not found. If this spikes, it's possible that the filesystem cache
        // just got pruned.
        metric!(counter("caches.file.miss") += 1, "cache" => name.as_ref());

        self.spawn_computation(request, cache_key, false).await
    }
}

async fn persist_tempfile(
    mut temp_file: NamedTempFile,
    cache_path: PathBuf,
) -> io::Result<std::fs::File> {
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
