use actix::{
    fut::{wrap_future, ActorFuture, WrapFuture},
    Actor, Context, Handler, MailboxError, Message, ResponseActFuture,
};
use futures::future::{Future, IntoFuture};
use std::{
    collections::BTreeMap,
    fs::create_dir_all,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use tempfile::NamedTempFile;

use symbolic::common::ByteView;

use crate::types::Scope;

#[derive(Debug, Clone)]
pub struct CacheActor<T: CacheItemRequest> {
    cache_dir: Option<PathBuf>,
    cache_items: BTreeMap<String, Arc<T::Item>>,
}

impl<T: CacheItemRequest> CacheActor<T> {
    pub fn new<P: AsRef<Path>>(cache_dir: Option<P>) -> Self {
        CacheActor {
            cache_dir: cache_dir.map(|x| x.as_ref().to_owned()),
            cache_items: BTreeMap::new(),
        }
    }

    fn get_scope_path(&self, scope: &Scope, cache_key: &str) -> Result<Option<PathBuf>, io::Error> {
        let dir = match self.cache_dir.as_ref() {
            Some(x) => x.join(scope.as_ref()),
            None => return Ok(None),
        };

        create_dir_all(&dir)?;
        Ok(Some(dir.join(cache_key)))
    }
}

impl<T: CacheItemRequest> Actor for CacheActor<T> {
    type Context = Context<Self>;
}

pub struct CacheKey {
    pub cache_key: String,
    pub scope: Scope,
}

impl AsRef<str> for Scope {
    fn as_ref(&self) -> &str {
        match *self {
            Scope::Scoped(ref s) => &s,
            Scope::Global => "global",
        }
    }
}

pub trait CacheItemRequest: 'static + Send {
    type Item: 'static + Send;
    type Error: 'static + From<MailboxError> + From<io::Error> + Send;

    fn get_cache_key(&self) -> CacheKey;
    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>>;
    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error>;
}

pub struct ComputeMemoized<T>(pub T);

impl<T: CacheItemRequest> Message for ComputeMemoized<T> {
    type Result = Result<T::Item, T::Error>;
}

impl<T: CacheItemRequest> Handler<ComputeMemoized<T>> for CacheActor<T> {
    type Result = ResponseActFuture<Self, T::Item, T::Error>;

    fn handle(&mut self, request: ComputeMemoized<T>, _ctx: &mut Self::Context) -> Self::Result {
        // XXX: Unsure if we need SyncArbiter here
        let file = tryfa!(NamedTempFile::new());

        let key = request.0.get_cache_key();

        for scope in &[key.scope.clone(), Scope::Global] {
            let path = tryfa!(self.get_scope_path(scope, &key.cache_key));

            if let Some(ref path) = path {
                if path.exists() {
                    let byteview = tryfa!(ByteView::open(path));
                    let item = tryfa!(request.0.load(scope.clone(), byteview));
                    return Box::new(wrap_future(Ok(item).into_future()));
                }
            }
        }

        let future = request.0.compute(file.path()).into_actor(self).and_then(
            move |new_scope, slf, _ctx| {
                let new_cache_path = tryfa!(slf.get_scope_path(&new_scope, &key.cache_key));
                let item = tryfa!(request
                    .0
                    .load(new_scope, tryfa!(ByteView::open(file.path()))));

                if let Some(ref cache_path) = new_cache_path {
                    tryfa!(file.persist(cache_path).map_err(|x| x.error));
                }

                Box::new(Ok(item).into())
            },
        );

        Box::new(future)
    }
}
