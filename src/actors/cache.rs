use actix::Actor;
use actix::Addr;
use actix::Context;
use actix::Handler;
use actix::MailboxError;
use actix::Message;
use actix::ResponseFuture;
use futures::future::result;
use futures::future::Future;
use std::collections::BTreeMap;
use std::io;
use std::io::Read;
use std::marker::PhantomData;
use std::path::Path;
use std::path::PathBuf;

use tempfile::NamedTempFile;

use symbolic::common::ByteView;

pub struct CacheActor<M: Actor> {
    cache_dir: PathBuf,
    cache_items: BTreeMap<String, Addr<M>>,
}

impl<M: Actor> CacheActor<M> {
    pub fn new<P: AsRef<Path>>(cache_dir: P) -> Self {
        CacheActor {
            cache_dir: cache_dir.as_ref().to_owned(),
            cache_items: BTreeMap::new(),
        }
    }
}

impl<M: Actor> Actor for CacheActor<M> {
    type Context = Context<Self>;
}

pub struct GetCacheKey;

impl Message for GetCacheKey {
    type Result = String;
}

pub struct Compute<T: CacheItem> {
    pub path: PathBuf,
    _phantom: PhantomData<T>,
}

impl<T: CacheItem> Compute<T> {
    fn new(path: PathBuf) -> Self {
        Compute {
            path,
            _phantom: PhantomData,
        }
    }
}

impl<T: CacheItem> Message for Compute<T> {
    type Result = Result<(), T::Error>;
}

pub struct LoadCache<T: CacheItem> {
    pub value: ByteView<'static>,
    _phantom: PhantomData<T>,
}

impl<T: CacheItem> LoadCache<T> {
    fn new(value: ByteView<'static>) -> Self {
        LoadCache {
            value,
            _phantom: PhantomData,
        }
    }
}

impl<T: CacheItem> Message for LoadCache<T> {
    type Result = Result<(), T::Error>;
}

pub trait CacheItem:
    'static
    + Send
    + Actor<Context = Context<Self>>
    + Handler<GetCacheKey>
    + Handler<Compute<Self>>
    + Handler<LoadCache<Self>>
{
    type Error: 'static + From<MailboxError> + From<io::Error> + Send;
}

pub struct ComputeMemoized<T: CacheItem>(pub T);

impl<T: CacheItem> Message for ComputeMemoized<T> {
    type Result = Result<Addr<T>, T::Error>;
}

impl<T: CacheItem> Handler<ComputeMemoized<T>> for CacheActor<T> {
    type Result = ResponseFuture<Addr<T>, T::Error>;

    fn handle(&mut self, item: ComputeMemoized<T>, _ctx: &mut Self::Context) -> Self::Result {
        // TODO: rewrite
        let item = item.0.start();
        let file = NamedTempFile::new().unwrap();

        let future = item
            .send(Compute::new(file.path().to_owned()))
            .flatten()
            .and_then(move |_| {
                let mut buf = vec![];
                // TODO: SyncArbiter
                let mut read = file.reopen().unwrap();
                result(read.read_to_end(&mut buf).map_err(From::from)).map(|_| buf)
            })
            .and_then(|buf| {
                item.send(LoadCache::new(ByteView::from_vec(buf)))
                    .map_err(From::from)
                    .map(|_| item)
            });

        Box::new(future)
    }
}
