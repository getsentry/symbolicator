use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use futures::future::{Future, IntoFuture, Shared, SharedError};
use futures::sync::oneshot;
use parking_lot::RwLock;
use sentry::configure_scope;
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::cache::{get_scope_path, Cache, CacheKey};
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

    fn get_cache_key(&self) -> CacheKey;
    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>>;
    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error>;
}

impl<T: CacheItemRequest> Cacher<T> {
    pub fn compute_memoized(
        &self,
        request: T,
    ) -> Box<dyn Future<Item = Arc<T::Item>, Error = Arc<T::Error>>> {
        let key = request.get_cache_key();
        let name = self.config.name();

        if let Some(channel) = self.current_computations.read().get(&key) {
            // A concurrent cache lookup was deduplicated.
            metric!(counter(&format!("caches.{}.channel.hit", name)) += 1);
            return Box::new(
                channel
                    .clone()
                    .map_err(|_: SharedError<oneshot::Canceled>| {
                        panic!("Oneshot channel cancelled! Race condition or system shutting down")
                    })
                    .and_then(|result| (*result).clone()),
            );
        }

        // A concurrent cache lookup is considered new. This does not imply a full cache miss.
        metric!(counter(&format!("caches.{}.channel.miss", name)) += 1);
        let config = self.config.clone();

        let (tx, rx) = oneshot::channel();

        let file = if let Some(ref dir) = config.cache_dir() {
            fs::create_dir_all(dir).and_then(|_| NamedTempFile::new_in(dir))
        } else {
            NamedTempFile::new()
        }
        .map_err(T::Error::from)
        .into_future();

        let result = file.and_then(clone!(key, |file| {
            for &scope in &[&key.scope, &Scope::Global] {
                let path = tryf!(get_scope_path(config.cache_dir(), &scope, &key.cache_key));

                let path = match path {
                    Some(x) => x,
                    None => continue,
                };

                let byteview = match tryf!(config.open_cachefile(&path)) {
                    Some(x) => x,
                    None => continue,
                };

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

                log::trace!(
                    "Loading existing cache item for {} at path {:?}",
                    name,
                    path
                );

                let item = tryf!(request.load(scope.clone(), byteview));
                return Box::new(Ok(item).into_future());
            }

            // A file was not found. If this spikes, it's possible that the filesystem cache
            // just got pruned.
            metric!(counter(&format!("caches.{}.file.miss", name)) += 1);

            // XXX: Unsure if we need SyncArbiter here
            Box::new(request.compute(file.path()).and_then(move |new_scope| {
                let new_cache_path = tryf!(get_scope_path(
                    config.cache_dir(),
                    &new_scope,
                    &key.cache_key
                ));

                if let Some(ref cache_path) = new_cache_path {
                    configure_scope(|scope| {
                        scope.set_extra(
                            &format!("cache.{}.cache_path", name),
                            format!("{:?}", cache_path).into(),
                        );
                    });

                    log::trace!("Creating cache item for {} at path {:?}", name, cache_path);
                }

                let byteview = tryf!(ByteView::open(file.path()));

                metric!(
                    time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
                    "hit" => "false"
                );

                let item = tryf!(request.load(new_scope, byteview));

                if let Some(ref cache_path) = new_cache_path {
                    tryf!(file.persist(cache_path).map_err(|x| x.error));
                }

                Box::new(Ok(item).into_future())
            })) as Box<dyn Future<Item = T::Item, Error = T::Error>>
        }));

        let current_computations = &self.current_computations;

        actix::spawn(
            result
                .then(clone!(key, current_computations, |result| {
                    current_computations.write().remove(&key);
                    tx.send(match result {
                        Ok(x) => Ok(Arc::new(x)),
                        Err(e) => Err(Arc::new(e)),
                    })
                    .map_err(|_| ())
                    .into_future()
                }))
                .sentry_hub_current(),
        );

        let channel = rx.shared();

        self.current_computations
            .write()
            .insert(key.clone(), channel.clone());

        Box::new(
            channel
                .map_err(|_: SharedError<oneshot::Canceled>| {
                    panic!("Oneshot channel cancelled! Race condition or system shutting down")
                })
                .and_then(|result| (*result).clone()),
        )
    }
}
