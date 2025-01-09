use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use super::shared_cache::{CacheStoreReason, SharedCacheRef};
use crate::utils::futures::CallOnDrop;

use super::{Cache, CacheEntry, CacheError, CacheKey, ExpirationTime, SharedCacheService};

type InMemoryItem<T> = (Instant, CacheEntry<T>);
type InMemoryCache<T> = moka::future::Cache<CacheKey, InMemoryItem<T>>;

/// Manages a filesystem cache of any kind of data that can be de/serialized from/to bytes.
///
/// Transparently performs cache lookups, downloads and cache stores via the [`CacheItemRequest`]
/// trait and associated types.
///
/// Internally deduplicates concurrent cache lookups (in-memory).
pub struct Cacher<T: CacheItemRequest> {
    config: Cache,

    /// An in-memory Cache for some items which also does request-coalescing when requesting items.
    cache: InMemoryCache<T::Item>,

    /// A [`HashSet`] of currently running cache refreshes.
    refreshes: Arc<Mutex<HashSet<CacheKey>>>,

    /// A service used to communicate with the shared cache.
    shared_cache: SharedCacheRef,
}

impl<T: CacheItemRequest> std::fmt::Debug for Cacher<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let refreshes = self
            .refreshes
            .try_lock()
            .map(|r| r.len())
            .unwrap_or_default();
        f.debug_struct("Cacher")
            .field("config", &self.config)
            .field("in-memory items", &self.cache.entry_count())
            .field("running refreshes", &refreshes)
            .field("shared_cache", &self.shared_cache)
            .finish()
    }
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

/// A struct implementing [`moka::Expiry`] that uses the [`InMemoryItem`] [`Instant`] as the explicit
/// expiration time.
struct CacheExpiration;

/// Returns the duration between the `current_time` and `target_time` in the future.
/// In case the `target_time` is already elapsed (it is in the past relative to `current_time`), this
/// will return `Some(ZERO)`.
fn saturating_duration_since(current_time: Instant, target_time: Instant) -> Option<Duration> {
    Some(
        target_time
            .checked_duration_since(current_time)
            .unwrap_or_default(),
    )
}

impl<T> moka::Expiry<CacheKey, InMemoryItem<T>> for CacheExpiration {
    fn expire_after_create(
        &self,
        _key: &CacheKey,
        value: &InMemoryItem<T>,
        current_time: Instant,
    ) -> Option<Duration> {
        saturating_duration_since(current_time, value.0)
    }

    fn expire_after_update(
        &self,
        _key: &CacheKey,
        value: &InMemoryItem<T>,
        current_time: Instant,
        _current_duration: Option<Duration>,
    ) -> Option<Duration> {
        saturating_duration_since(current_time, value.0)
    }
}

impl<T: CacheItemRequest> Cacher<T> {
    pub fn new(config: Cache, shared_cache: SharedCacheRef) -> Self {
        let cache = InMemoryCache::builder()
            .max_capacity(config.in_memory_capacity)
            .name(config.name().as_ref())
            .expire_after(CacheExpiration)
            // NOTE: we count all the bookkeeping structures to the weight as well
            .weigher(|_k, v| {
                let value_size =
                    v.1.as_ref()
                        .map_or(0, T::weight)
                        .max(std::mem::size_of::<CacheError>() as u32);
                std::mem::size_of::<(CacheKey, Instant)>() as u32 + value_size
            })
            .build();

        Cacher {
            config,
            cache,
            refreshes: Default::default(),
            shared_cache,
        }
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
    const VERSIONS: CacheVersions;

    /// Invoked to compute an instance of this item and put it at the given location in the file
    /// system. This is used to populate the cache for a previously missing element.
    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry>;

    /// Loads an existing element from the cache.
    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item>;

    /// The "cost" of keeping this item in the in-memory cache.
    fn weight(item: &Self::Item) -> u32 {
        std::mem::size_of_val(item) as u32
    }

    /// Allows avoiding the shared cache per item.
    fn use_shared_cache(&self) -> bool {
        true
    }
}

impl<T: CacheItemRequest> Cacher<T> {
    fn shared_cache(&self, request: &T) -> Option<&SharedCacheService> {
        request
            .use_shared_cache()
            .then(|| self.shared_cache.get())
            .flatten()
    }

    /// Compute an item.
    ///
    /// The item is computed using [`T::compute`](CacheItemRequest::compute), and saved in the cache
    /// if one is configured. The `is_refresh` flag is used only to tag computation metrics.
    ///
    /// This method does not take care of ensuring the computation only happens once even
    /// for concurrent requests, see the public [`Cacher::compute_memoized`] for this.
    async fn compute(&self, request: T, key: &CacheKey, is_refresh: bool) -> CacheEntry<T::Item> {
        let name = self.config.name();
        let cache_path = key.cache_path(T::VERSIONS.current);
        let mut temp_file = self.config.tempfile()?;

        let shared_cache = self.shared_cache(&request);
        let shared_cache_hit = if let Some(shared_cache) = shared_cache {
            let temp_fd = tokio::fs::File::from_std(temp_file.reopen()?);
            shared_cache.fetch(name, &cache_path, temp_fd).await
        } else {
            false
        };

        let mut entry = if shared_cache_hit {
            let byte_view = ByteView::map_file_ref(temp_file.as_file())?;
            Ok(byte_view)
        } else {
            Err(CacheError::NotFound)
        };

        if entry.is_err() {
            metric!(counter("caches.computation") += 1, "cache" => name.as_ref());
            match request.compute(&mut temp_file).await {
                Ok(()) => {
                    // Now we have written the data to the tempfile we can mmap it, persisting it later
                    // is fine as it does not move filesystem boundaries there.
                    let byte_view = ByteView::map_file_ref(temp_file.as_file())?;
                    entry = Ok(byte_view);
                }
                Err(CacheError::InternalError) => {
                    // If there was an `InternalError` during computation (for instance because
                    // of an io error), we return immediately without writing the error
                    // or persisting the temp file.
                    return Err(CacheError::InternalError);
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
            let cache_path = cache_dir.join(&cache_path);

            sentry::configure_scope(|scope| {
                scope.set_extra(
                    &format!("cache.{name}.cache_path"),
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
                    time_raw("caches.file.size") = byte_view.len() as u64,
                    "hit" => "false",
                    "is_refresh" => &is_refresh.to_string(),
                    "cache" => name.as_ref(),
                );
            }

            tracing::trace!("Creating {name} at path {:?}", cache_path.display());

            persist_tempfile(temp_file, &cache_path)?;

            // Clean up old versions
            for version in 0..T::VERSIONS.current {
                let item_path = key.cache_path(version);

                if let Err(e) = fs::remove_file(cache_dir.join(&item_path)) {
                    // `NotFound` errors are no cause for concern—it's likely that not all fallback versions exist anymore.
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::error!(
                            error = &e as &dyn std::error::Error,
                            path = item_path,
                            "Failed to remove old cache file"
                        );
                    }
                }
            }

            #[cfg(debug_assertions)]
            {
                let mut cache_path = cache_path;
                // NOTE: we only create the metadata file once, but do not regularly touch it for now
                cache_path.set_extension("txt");
                if let Err(err) = std::fs::write(cache_path, key.metadata()) {
                    tracing::error!(error = &err as &dyn std::error::Error);
                }
            }
        };

        // TODO: Not handling negative caches probably has a huge perf impact.  Need to
        // figure out negative caches.  Maybe put them in redis with a TTL?
        if !shared_cache_hit {
            if let Ok(byteview) = &entry {
                if let Some(shared_cache) = shared_cache {
                    shared_cache.store(name, &cache_path, byteview.clone(), CacheStoreReason::New);
                }
            }
        }

        entry.and_then(|byteview| request.load(byteview))
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
        metric!(counter("caches.access") += 1, "cache" => name.as_ref());

        let init = Box::pin(async {
            // cache_path is None when caching is disabled.
            if let Some(cache_dir) = self.config.cache_dir() {
                let shared_cache = self.shared_cache(&request);
                let versions = std::iter::once(T::VERSIONS.current)
                    .chain(T::VERSIONS.fallbacks.iter().copied());

                for version in versions {
                    let is_current_version = version == T::VERSIONS.current;
                    // try the new cache key first, then fall back to the old cache key
                    let item = match lookup_local_cache(
                        &self.config,
                        shared_cache,
                        cache_dir,
                        &cache_key.cache_path(version),
                        is_current_version,
                    ) {
                        Err(CacheError::NotFound) => continue,
                        Err(err) => {
                            let item = Err(err);
                            let expiration = ExpirationTime::for_fresh_status(&self.config, &item);
                            return (expiration.as_instant(), item);
                        }
                        Ok(item) => (item.0, item.1.and_then(|byteview| request.load(byteview))),
                    };

                    if !is_current_version {
                        // we have found an outdated cache that we will use right away,
                        // and we will kick off a recomputation for the `current` cache version
                        // in a deduplicated background task, which we will not await
                        metric!(
                            counter("caches.file.fallback") += 1,
                            "version" => &version.to_string(),
                            "cache" => name.as_ref(),
                        );
                        self.spawn_refresh(cache_key.clone(), request);
                    }

                    return item;
                }
            }

            // A file was not found. If this spikes, it's possible that the filesystem cache
            // just got pruned.
            metric!(counter("caches.file.miss") += 1, "cache" => name.as_ref());

            let item = self
                .compute(request, &cache_key, false)
                // NOTE: We have seen this deadlock with an SDK that was deadlocking on
                // out-of-order Scope pops.
                // To guarantee that this does not happen is really the responsibility of
                // the caller, though to be safe, we will just bind a fresh hub here.
                .bind_hub(Hub::new_from_top(Hub::current()))
                .await;

            // we just created a fresh cache, so use the initial expiration times
            let expiration = ExpirationTime::for_fresh_status(&self.config, &item);

            (expiration.as_instant(), item)
        });

        let entry = self
            .cache
            .entry_by_ref(&cache_key)
            .or_insert_with(init)
            .await;

        if !entry.is_fresh() {
            metric!(counter("caches.memory.hit") += 1, "cache" => name.as_ref());
        }
        entry.into_value().1
    }

    fn spawn_refresh(&self, cache_key: CacheKey, request: T) {
        let name = self.config.name();

        let mut refreshes = self.refreshes.lock().unwrap();
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
                refreshes.lock().unwrap().remove(&key);
            })
        };

        refreshes.insert(cache_key.clone());
        drop(refreshes);

        tracing::trace!(
            "Spawning deduplicated {} computation for path {:?}",
            name,
            cache_key.cache_path(T::VERSIONS.current)
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

            let item = this.compute(request, &cache_key, true).await;

            // we just created a fresh cache, so use the initial expiration times
            let expiration = ExpirationTime::for_fresh_status(&this.config, &item);
            let value = (expiration.as_instant(), item);

            // refresh the memory cache with the newly refreshed result
            this.cache.insert(cache_key, value).await;

            transaction.finish();
        };
        tokio::spawn(task.bind_hub(Hub::new_from_top(Hub::current())));
    }
}

/// Look up an item in the file system cache and load it if available.
///
/// Returns `Err(NotFound)` if the cache item does not exist or needs to be re-computed.
/// Otherwise returns another `CacheEntry`, which itself can be `NotFound`.
fn lookup_local_cache(
    config: &Cache,
    shared_cache: Option<&SharedCacheService>,
    cache_dir: &Path,
    cache_key: &str,
    is_current_version: bool,
) -> CacheEntry<(Instant, CacheEntry<ByteView<'static>>)> {
    let name = config.name();

    let item_path = cache_dir.join(cache_key);
    tracing::trace!("Trying {} cache at path {}", name, item_path.display());
    let _scope = Hub::current().push_scope();
    sentry::configure_scope(|scope| {
        scope.set_extra(
            &format!("cache.{name}.cache_path"),
            item_path.to_string_lossy().into(),
        );
    });
    let (entry, expiration) = config
        .open_cachefile(&item_path)?
        .ok_or(CacheError::NotFound)?;

    // store things into the shared cache when:
    // - we have a positive cache
    // - that has the latest version (we don’t want to upload old versions)
    // - we refreshed the local cache time, so we also refresh the shared cache time.
    let needs_reupload = expiration.was_touched();
    // FIXME: let-chains would be nice here :-)
    if is_current_version && needs_reupload {
        if let Ok(byteview) = &entry {
            if let Some(shared_cache) = shared_cache {
                shared_cache.store(name, cache_key, byteview.clone(), CacheStoreReason::Refresh);
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

    Ok((expiration.as_instant(), entry))
}

fn persist_tempfile(
    mut temp_file: NamedTempFile,
    cache_path: &Path,
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

        if let Err(e) = std::fs::create_dir_all(parent) {
            sentry::with_scope(
                |scope| scope.set_extra("path", parent.display().to_string().into()),
                || tracing::error!("Failed to create cache directory: {:?}", e),
            );
            if retries > MAX_RETRIES {
                return Err(e);
            }
            continue;
        }

        match temp_file.persist(cache_path) {
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
