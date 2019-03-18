use std::{
    collections::BTreeMap,
    fs::{self, create_dir_all},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use actix::{
    fut::{ActorFuture, WrapFuture},
    Actor, AsyncContext, Context, Handler, Message, ResponseActFuture,
};

use futures::{
    future::{Future, IntoFuture, Shared, SharedError},
    sync::oneshot,
};

use symbolic::common::ByteView;

use tempfile::NamedTempFile;

use crate::types::Scope;

// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<Result<Arc<T>, Arc<E>>>>;

#[derive(Clone)]
pub struct CacheActor<T: CacheItemRequest> {
    cache_dir: Option<PathBuf>,

    current_computations: BTreeMap<CacheKey, ComputationChannel<T::Item, T::Error>>,
}

impl<T: CacheItemRequest> CacheActor<T> {
    pub fn new<P: AsRef<Path>>(cache_dir: Option<P>) -> Self {
        CacheActor {
            cache_dir: cache_dir.map(|x| x.as_ref().to_owned()),
            current_computations: BTreeMap::new(),
        }
    }
}

impl<T: CacheItemRequest> Actor for CacheActor<T> {
    type Context = Context<Self>;
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct CacheKey {
    pub cache_key: String,
    pub scope: Scope,
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

        let channel = if let Some(channel) = self.current_computations.get(&key) {
            channel.clone()
        } else {
            let cache_dir = self.cache_dir.clone();

            let (tx, rx) = oneshot::channel();

            let file = NamedTempFile::new().map_err(T::Error::from).into_future();

            let result = file.and_then(clone!(key, |file| {
                for &scope in &[&key.scope, &Scope::Global] {
                    let path = tryf!(get_scope_path(
                        cache_dir.as_ref().map(|x| &**x),
                        scope,
                        &key.cache_key
                    ));

                    if let Some(ref path) = path {
                        if path.exists() {
                            let _ = tryf!(fs::OpenOptions::new()
                                .append(true)
                                .truncate(false)
                                .open(&path));
                            let byteview = tryf!(ByteView::open(path));
                            let item = tryf!(request.0.load(scope.clone(), byteview));
                            return Box::new(Ok(item).into_future());
                        }
                    }
                }

                // XXX: Unsure if we need SyncArbiter here
                Box::new(request.0.compute(file.path()).and_then(move |new_scope| {
                    let new_cache_path = tryf!(get_scope_path(
                        cache_dir.as_ref().map(|x| &**x),
                        &new_scope,
                        &key.cache_key
                    ));
                    let item = tryf!(request
                        .0
                        .load(new_scope, tryf!(ByteView::open(file.path()))));

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

fn get_scope_path(
    cache_dir: Option<&Path>,
    scope: &Scope,
    cache_key: &str,
) -> Result<Option<PathBuf>, io::Error> {
    let dir = match cache_dir {
        Some(x) => x.join(scope.as_ref()),
        None => return Ok(None),
    };

    create_dir_all(&dir)?;
    Ok(Some(dir.join(cache_key)))
}
