use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use futures::future::{Future, IntoFuture, Shared};
use futures::sync::oneshot;
use parking_lot::RwLock;
use sentry::configure_scope;
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::cache::{get_scope_path, Cache, CacheKey, CacheStatus};
use crate::sentry::SentryFutureExt;
use crate::types::Scope;

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
#[derive(Clone)]
pub struct Cacher<T: CacheItemRequest> {
    config: Cache,

    /// Used for deduplicating cache lookups.
    current_computations: ComputationMap<T::Item, T::Error>,
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

    fn get_lookup_cache_keys(&self) -> Vec<CacheKey> {
        let key = self.get_cache_key();
        let mut global_key = key.clone();
        global_key.scope = Scope::Global;

        vec![key, global_key]
    }

    /// Invoked to compute an instance of this item and put it at the given location in the file
    /// system. This is used to populate the cache for a previously missing element.
    fn compute(
        &self,
        path: &Path,
    ) -> Box<dyn Future<Item = (CacheStatus, Option<CacheKey>), Error = Self::Error>>;

    /// Determines whether this item should be loaded.
    fn should_load(&self, _data: &[u8]) -> bool {
        true
    }

    /// Loads an existing element from the cache.
    fn load(
        &self,
        cache_key: Option<CacheKey>,
        status: CacheStatus,
        data: ByteView<'static>,
    ) -> Self::Item;
}

impl<T: CacheItemRequest> Cacher<T> {
    pub fn compute_memoized(
        &self,
        request: T,
    ) -> Box<dyn Future<Item = Arc<T::Item>, Error = Arc<T::Error>>> {
        let name = self.config.name();

        let config = self.config.clone();
        let (tx, rx) = oneshot::channel();

        let file_result = match config.cache_dir() {
            Some(ref dir) => fs::create_dir_all(dir).and_then(|_| NamedTempFile::new_in(dir)),
            None => NamedTempFile::new(),
        };

        let compute_future = file_result
            .map_err(T::Error::from)
            .into_future()
            .and_then(move |file| {
                for key in request.get_lookup_cache_keys().into_iter() {
                    let path = tryf!(get_scope_path(
                        config.cache_dir(),
                        &key.scope,
                        &key.cache_key
                    ));

                    let path = match path {
                        Some(x) => x,
                        None => continue,
                    };

                    let byteview = match tryf!(config.open_cachefile(&path)) {
                        Some(x) => x,
                        None => continue,
                    };

                    let status = CacheStatus::from_content(&byteview);
                    if status == CacheStatus::Positive && !request.should_load(&byteview) {
                        log::trace!("Discarding {} at path {:?}", name, path);
                        metric!(counter(&format!("caches.{}.file.discarded", name)) += 1);
                        continue;
                    }

                    // This is also reported for "negative cache hits": When we cached the 404 response from a
                    // server as empty file.
                    metric!(counter(&format!("caches.{}.file.hit", name)) += 1);

                    metric!(
                        time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
                        "hit" => "true"
                    );

                    configure_scope(|scope| {
                        scope.set_extra(
                            &format!("cache.{}.cache_path", name),
                            format!("{:?}", path).into(),
                        );
                    });

                    log::trace!("Loading {} at path {:?}", name, path);

                    let item = request.load(Some(key), status, byteview);

                    return Box::new(Ok(item).into_future());
                }

                // A file was not found. If this spikes, it's possible that the filesystem cache
                // just got pruned.
                metric!(counter(&format!("caches.{}.file.miss", name)) += 1);

                let future = request.compute(file.path()).and_then(
                    move |(status, new_cache_key)| {
                        let new_cache_path = if let Some(ref new_cache_key) = new_cache_key {
                            tryf!(get_scope_path(
                                config.cache_dir(),
                                &new_cache_key.scope,
                                &new_cache_key.cache_key
                            ))
                        } else {
                            None
                        };

                        if let Some(ref cache_path) = new_cache_path {
                            configure_scope(|scope| {
                                scope.set_extra(
                                    &format!("cache.{}.cache_path", name),
                                    format!("{:?}", cache_path).into(),
                                );
                            });

                            log::trace!("Creating {} at path {:?}", name, cache_path);
                        }

                        let byteview = tryf!(ByteView::open(file.path()));

                        metric!(
                            counter(&format!("caches.{}.file.write", name)) += 1,
                            "status" => status.as_ref(),
                        );

                        metric!(
                            time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
                            "hit" => "false"
                        );

                        let item = request.load(new_cache_key, status, byteview);

                        if let Some(ref cache_path) = new_cache_path {
                            tryf!(status.persist_item(cache_path, file));
                        }

                        Box::new(Ok(item).into_future())
                    },
                );

                Box::new(future) as Box<dyn Future<Item = T::Item, Error = T::Error>>
            })
            .then(move |result| {
                tx.send(match result {
                    // TODO: Arc no longer necessary
                    Ok(x) => Ok(Arc::new(x)),
                    Err(e) => Err(Arc::new(e)),
                })
                .map_err(|_| ())
                .into_future()
            })
            .sentry_hub_current();

        actix::spawn(compute_future);

        let item_future = rx
            .shared() // TODO: No longer necessary
            .map_err(|_| {
                panic!("Oneshot channel cancelled! Race condition or system shutting down")
            })
            .and_then(|result| (*result).clone());

        Box::new(item_future)
    }
}
