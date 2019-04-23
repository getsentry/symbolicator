use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use actix::fut::{ActorFuture, WrapFuture};
use actix::{Actor, AsyncContext, Context, Handler, Message, ResponseActFuture};
use futures::future::{Future, IntoFuture, Shared, SharedError};
use futures::sync::oneshot;
use symbolic::common::ByteView;
use tempfile::NamedTempFile;

use crate::cache::{get_scope_path, Cache, CacheKey};
use crate::types::Scope;

// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<Result<Arc<T>, Arc<E>>>>;

/// Manages a filesystem cache of any kind of data that can be serialized into bytes and read from
/// it:
///
/// - Object files
/// - Symcaches
/// - CFI caches
///
/// Handles a single message `ComputeMemoized` that transparently performs cache lookups,
/// downloads and cache stores via the `CacheItemRequest` trait and associated types.
///
/// Internally deduplicates concurrent cache lookups (in-memory).
#[derive(Clone)]
pub struct CacheActor<T: CacheItemRequest> {
    config: Cache,

    /// Used for deduplicating cache lookups.
    current_computations: BTreeMap<CacheKey, ComputationChannel<T::Item, T::Error>>,
}

impl<T: CacheItemRequest> CacheActor<T> {
    pub fn new(config: Cache) -> Self {
        CacheActor {
            config,
            current_computations: BTreeMap::new(),
        }
    }
}

impl<T: CacheItemRequest> Actor for CacheActor<T> {
    type Context = Context<Self>;
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

pub struct ComputeMemoized<T>(pub T);

impl<T: CacheItemRequest> Message for ComputeMemoized<T> {
    type Result = Result<Arc<T::Item>, Arc<T::Error>>;
}

impl<T: CacheItemRequest> Handler<ComputeMemoized<T>> for CacheActor<T> {
    type Result = ResponseActFuture<Self, Arc<T::Item>, Arc<T::Error>>;

    fn handle(&mut self, request: ComputeMemoized<T>, ctx: &mut Self::Context) -> Self::Result {
        let key = request.0.get_cache_key();
        let name = self.config.name();

        let channel = if let Some(channel) = self.current_computations.get(&key) {
            // A concurrent cache lookup was deduplicated.
            metric!(counter(&format!("caches.{}.channel.hit", name)) += 1);
            channel.clone()
        } else {
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
                    let path = tryf!(get_scope_path(
                        config.cache_dir(),
                        &scope,
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

                    // This is also reported for "negative cache hits": When we cached the 404 response from a
                    // server as empty file.
                    metric!(counter(&format!("caches.{}.file.hit", name)) += 1);

                    metric!(
                        time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
                        "hit" => "true"
                    );

                    let item = tryf!(request.0.load(scope.clone(), byteview));
                    return Box::new(Ok(item).into_future());
                }

                // A file was not found. If this spikes, it's possible that the filesystem cache
                // just got pruned.
                metric!(counter(&format!("caches.{}.file.miss", name)) += 1);

                // XXX: Unsure if we need SyncArbiter here
                Box::new(request.0.compute(file.path()).and_then(move |new_scope| {
                    let new_cache_path = tryf!(get_scope_path(
                        config.cache_dir(),
                        &new_scope,
                        &key.cache_key
                    ));

                    let byteview = tryf!(ByteView::open(file.path()));

                    metric!(
                        time_raw(&format!("caches.{}.file.size", name)) = byteview.len() as u64,
                        "hit" => "false"
                    );

                    let item = tryf!(request.0.load(new_scope, byteview));

                    if let Some(ref cache_path) = new_cache_path {
                        tryf!(file.persist(cache_path).map_err(|x| x.error));
                    }

                    Box::new(Ok(item).into_future())
                })) as Box<dyn Future<Item = T::Item, Error = T::Error>>
            }));

            ctx.spawn(
                result
                    .into_actor(self)
                    .then(clone!(key, |result, slf, _ctx| {
                        slf.current_computations.remove(&key);

                        tx.send(match result {
                            Ok(x) => Ok(Arc::new(x)),
                            Err(e) => Err(Arc::new(e)),
                        })
                        .map_err(|_| ())
                        .into_future()
                        .into_actor(slf)
                    })),
            );

            let channel = rx.shared();

            self.current_computations
                .insert(key.clone(), channel.clone());
            channel
        };

        Box::new(
            channel
                .map_err(|_: SharedError<oneshot::Canceled>| {
                    panic!("Oneshot channel cancelled! Race condition or system shutting down")
                })
                .and_then(|result| (*result).clone())
                .into_actor(self),
        )
    }
}

handle_sentry_actix_message!(<T: CacheItemRequest>, CacheActor<T>, ComputeMemoized<T>);
