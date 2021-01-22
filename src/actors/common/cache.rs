use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::channel::oneshot;
use futures::future::{self, FutureExt, Shared, TryFutureExt};
use parking_lot::Mutex;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::cache::{get_scope_path, Cache, CacheKey, CacheStatus};
use crate::types::Scope;
use crate::utils::futures::{spawn_compat, BoxedFuture, CallOnDrop};

/// Result from [`Cacher::compute_memoized`].
type CacheResultFuture<T, E> = BoxedFuture<Result<Arc<T>, Arc<E>>>;

// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<Result<Arc<T>, Arc<E>>>>;

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
}

impl<T: CacheItemRequest> Clone for Cacher<T> {
    fn clone(&self) -> Self {
        // https://github.com/rust-lang/rust/issues/26925
        Cacher {
            config: self.config.clone(),
            current_computations: self.current_computations.clone(),
        }
    }
}

impl<T: CacheItemRequest> Cacher<T> {
    pub fn new(config: Cache) -> Self {
        Cacher {
            config,
            current_computations: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn tempfile(&self) -> io::Result<NamedTempFile> {
        self.config.tempfile()
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (scope {})", self.cache_key, self.scope)
    }
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
            Self::Temp(ref temp) => &temp,
            Self::Cached(ref buf) => &buf,
        }
    }
}

impl AsRef<Path> for CachePath {
    #[inline]
    fn as_ref(&self) -> &Path {
        match *self {
            Self::Temp(ref temp) => &temp,
            Self::Cached(ref buf) => &buf,
        }
    }
}

pub trait CacheItemRequest: 'static + Send {
    type Item: 'static + Send + Sync;

    // XXX: Probably should have our own concrete error type for cacheactor instead of forcing our
    // ioerrors into other errors
    type Error: 'static + From<io::Error> + Send + Sync;

    /// Returns the key by which this item is cached.
    fn get_cache_key(&self) -> CacheKey;

    /// Invoked to compute an instance of this item and put it at the given location in the file
    /// system. This is used to populate the cache for a previously missing element.
    fn compute(&self, path: &Path) -> BoxedFuture<Result<CacheStatus, Self::Error>>;

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
    fn lookup_cache(
        &self,
        request: &T,
        key: &CacheKey,
        path: &Path,
    ) -> Result<Option<T::Item>, T::Error> {
        let name = self.config.name();
        sentry::configure_scope(|scope| {
            scope.set_extra(
                &format!("cache.{}.cache_path", name),
                format!("{:?}", path).into(),
            );
        });

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
        let item = request.load(key.scope.clone(), status, byteview, CachePath::Cached(path));
        Ok(Some(item))
    }

    /// Compute an item.
    ///
    /// If the item is in the file system cache, it is returned immediately. Otherwise, it
    /// is computed using [`T::compute`](CacheItemRequest::compute), and then persisted to
    /// the cache.
    ///
    /// This method does not take care of ensuring the computation only happens once even
    /// for concurrent requests, see the public [`Cacher::compute_memoized`] for this.
    fn compute(&self, request: T, key: CacheKey) -> BoxedFuture<Result<T::Item, T::Error>> {
        // cache_path is None when caching is disabled.
        let cache_path = get_scope_path(self.config.cache_dir(), &key.scope, &key.cache_key);
        if let Some(ref path) = cache_path {
            if let Some(item) = tryf03pin!(self.lookup_cache(&request, &key, &path)) {
                return Box::pin(future::ok(item));
            }
        }

        let name = self.config.name();
        let key = request.get_cache_key();

        // A file was not found. If this spikes, it's possible that the filesystem cache
        // just got pruned.
        metric!(counter(&format!("caches.{}.file.miss", name)) += 1);

        let temp_file = tryf03pin!(self.tempfile());

        let future =
            request
                .compute(temp_file.path())
                .and_then(move |status: CacheStatus| async move {
                    if let Some(ref cache_path) = cache_path {
                        sentry::configure_scope(|scope| {
                            scope.set_extra(
                                &format!("cache.{}.cache_path", name),
                                cache_path.to_string_lossy().into(),
                            );
                        });

                        log::trace!("Creating {} at path {:?}", name, cache_path);
                    }

                    let byteview = ByteView::open(temp_file.path())?;

                    metric!(
                        counter(&format!("caches.{}.file.write", name)) += 1,
                        "status" => status.as_ref(),
                    );
                    metric!(
                        time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
                        "hit" => "false"
                    );

                    let path = match cache_path {
                        Some(ref cache_path) => {
                            status.persist_item(cache_path, temp_file)?;
                            CachePath::Cached(cache_path.to_path_buf())
                        }
                        None => CachePath::Temp(temp_file.into_temp_path()),
                    };

                    Ok(request.load(key.scope.clone(), status, byteview, path))
                });

        Box::pin(future)
    }

    /// Creates a shareable channel that computes an item.
    fn create_channel(&self, request: T, key: CacheKey) -> ComputationChannel<T::Item, T::Error> {
        let (sender, receiver) = oneshot::channel();

        let slf = self.clone();
        let current_computations = self.current_computations.clone();
        let remove_computation_token = CallOnDrop::new(clone!(key, || {
            current_computations.lock().remove(&key);
        }));

        // Run the computation and wrap the result in Arcs to make them clonable.
        let channel = async move {
            let result = match slf.compute(request, key).await {
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
        spawn_compat(channel);

        receiver.shared()
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
    pub fn compute_memoized(&self, request: T) -> CacheResultFuture<T::Item, T::Error> {
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
                let channel = self.create_channel(request, key.clone());
                let evicted = current_computations.insert(key.clone(), channel.clone());
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
}
