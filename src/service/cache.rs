use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use futures::future::{self, Future, Shared};
use futures::sync::oneshot;
use parking_lot::RwLock;
use sentry::Hub;
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::cache::{get_scope_path, Cache, CacheKey, CacheStatus};
use crate::types::Scope;
use crate::utils::sentry::SentryFutureExt;

// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<Result<Arc<T>, Arc<E>>>>;

type ComputationMap<T, E> = Arc<RwLock<BTreeMap<CacheKey, ComputationChannel<T, E>>>>;

/// Manages a filesystem cache of any kind of data that can be serialized into bytes and read from
/// it:
///
/// - Object files
/// - Symcaches
/// - CFI caches
///
/// Transparently performs cache lookups, downloads and cache stores via the `CacheItemRequest`
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
            current_computations: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} (scope {})", self.cache_key, self.scope)
    }
}

pub trait CacheItemRequest: 'static + Send {
    type Item: 'static + Send;

    // XXX: Probably should have our own concrete error type for cacheactor instead of forcing our
    // ioerrors into other errors
    type Error: 'static + From<io::Error> + Send;

    /// Returns the key by which this item is cached.
    fn get_cache_key(&self) -> CacheKey;

    /// Invoked to compute an instance of this item and put it at the given location in the file
    /// system. This is used to populate the cache for a previously missing element.
    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>>;

    /// Determines whether this item should be loaded.
    fn should_load(&self, _data: &[u8]) -> bool {
        true
    }

    /// Loads an existing element from the cache.
    fn load(&self, scope: Scope, status: CacheStatus, data: ByteView<'static>) -> Self::Item;
}

impl<T: CacheItemRequest> Cacher<T> {
    /// Look up an item in the file system cache and load it if available.
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

        log::trace!("Loading {} at path {:?}", name, path);
        let item = request.load(key.scope.clone(), status, byteview);
        Ok(Some(item))
    }

    /// Compute an item.
    ///
    /// If the item is in the file system cache, it is returned immediately. Otherwise, it is
    /// computed using `T::compute`, and then persisted to the cache.
    fn compute(
        &self,
        request: T,
        key: CacheKey,
    ) -> Box<dyn Future<Item = T::Item, Error = T::Error>> {
        let cache_path = get_scope_path(self.config.cache_dir(), &key.scope, &key.cache_key);
        if let Some(ref path) = cache_path {
            if let Some(item) = tryf!(self.lookup_cache(&request, &key, &path)) {
                return Box::new(future::ok(item));
            }
        }

        let name = self.config.name();
        let key = request.get_cache_key();

        // A file was not found. If this spikes, it's possible that the filesystem cache
        // just got pruned.
        metric!(counter(&format!("caches.{}.file.miss", name)) += 1);

        let temp_file = if let Some(ref path) = cache_path {
            let dir = path.parent().unwrap();
            tryf!(fs::create_dir_all(dir));
            tryf!(NamedTempFile::new_in(dir))
        } else {
            tryf!(NamedTempFile::new())
        };

        let future = request.compute(temp_file.path()).and_then(move |status| {
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

            let item = request.load(key.scope.clone(), status, byteview);
            if let Some(ref cache_path) = cache_path {
                status.persist_item(cache_path, temp_file)?;
            }

            Ok(item)
        });

        Box::new(future)
    }

    /// Creates a shareable channel that computes an item.
    fn create_channel(&self, request: T, key: CacheKey) -> ComputationChannel<T::Item, T::Error> {
        let (sender, receiver) = oneshot::channel();

        let slf = self.clone();
        let current_computations = self.current_computations.clone();

        // Run the computation and wrap the result in Arcs to make them clonable.
        let channel = future::lazy(clone!(key, || slf.compute(request, key)))
            .then(move |result| {
                current_computations.write().remove(&key);
                sender
                    .send(match result {
                        Ok(item) => Ok(Arc::new(item)),
                        Err(error) => Err(Arc::new(error)),
                    })
                    .map_err(|_| ())
            })
            .bind_hub(Hub::new_from_top(Hub::main()));

        actix_rt::spawn(channel);
        receiver.shared()
    }

    /// Computes an item by loading from or populating the cache.
    ///
    /// The actual computation is deduplicated between concurrent requests. Finally, the result is
    /// inserted into the cache and all subsequent calls fetch from the cache.
    pub fn compute_memoized(
        &self,
        request: T,
    ) -> Box<dyn Future<Item = Arc<T::Item>, Error = Arc<T::Error>>> {
        let key = request.get_cache_key();
        let name = self.config.name();

        let current_computations = &self.current_computations;
        let channel_opt = current_computations.read().get(&key).cloned();

        let channel = if let Some(channel) = channel_opt {
            // A concurrent cache lookup was deduplicated.
            metric!(counter(&format!("caches.{}.channel.hit", name)) += 1);
            channel.clone()
        } else {
            // A concurrent cache lookup is considered new. This does not imply a cache miss.
            metric!(counter(&format!("caches.{}.channel.miss", name)) += 1);
            let channel = self.create_channel(request, key.clone());
            current_computations
                .write()
                .insert(key.clone(), channel.clone());
            channel
        };

        let future = channel
            .map_err(move |_| panic!("{} computation channel dropped", name))
            .and_then(|shared| (*shared).clone());

        Box::new(future)
    }
}
