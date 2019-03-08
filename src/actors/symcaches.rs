use std::{
    fs::File,
    io::{self, BufWriter},
    path::Path,
    sync::Arc,
};

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};

use failure::{Fail, ResultExt};

use futures::{future::Future, lazy};

use symbolic::{common::ByteView, symcache};

use tokio_threadpool::ThreadPool;

use crate::{
    actors::{
        cache::{CacheActor, CacheItemRequest, CacheKey, ComputeMemoized},
        objects::{FetchObject, ObjectsActor},
    },
    types::{FileType, ObjectId, Scope, SourceConfig},
};

#[derive(Fail, Debug, Clone, Copy)]
pub enum SymCacheErrorKind {
    #[fail(display = "failed to fetch objects")]
    Fetching,

    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "failed sending message to objects actor")]
    Mailbox,

    #[fail(display = "failed to parse symcache during download")]
    Parse,

    #[fail(display = "symcache not found")]
    NotFound,
}

symbolic::common::derive_failure!(
    SymCacheError,
    SymCacheErrorKind,
    doc = "Errors happening while generating a symcache"
);

impl From<io::Error> for SymCacheError {
    fn from(e: io::Error) -> Self {
        e.context(SymCacheErrorKind::Io).into()
    }
}

pub struct SymCacheActor {
    symcaches: Addr<CacheActor<FetchSymCacheInternal>>,
    objects: Addr<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl Actor for SymCacheActor {
    type Context = Context<Self>;
}

impl SymCacheActor {
    pub fn new(
        symcaches: Addr<CacheActor<FetchSymCacheInternal>>,
        objects: Addr<ObjectsActor>,
    ) -> Self {
        let threadpool = Arc::new(ThreadPool::new());

        SymCacheActor {
            symcaches,
            objects,
            threadpool,
        }
    }
}

#[derive(Clone)]
pub struct SymCache {
    inner: Option<ByteView<'static>>,
    scope: Scope,
    request: FetchSymCacheInternal,
}

impl SymCache {
    pub fn get_symcache(&self) -> Result<symcache::SymCache<'_>, SymCacheError> {
        let bytes = self.inner.as_ref().ok_or(SymCacheErrorKind::NotFound)?;
        Ok(symcache::SymCache::parse(bytes).context(SymCacheErrorKind::Parse)?)
    }
}

#[derive(Clone)]
pub struct FetchSymCacheInternal {
    request: FetchSymCache,
    objects: Addr<ObjectsActor>,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCache;
    type Error = SymCacheError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.request.identifier.get_cache_key(),
            scope: self.request.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let objects = self.objects.clone();

        let object = objects
            .send(FetchObject {
                filetypes: FileType::debug_types()
                    .iter()
                    .cloned()
                    .chain(FileType::code_types().iter().cloned())
                    .chain(Some(FileType::Breakpad).into_iter())
                    .collect(),
                identifier: self.request.identifier.clone(),
                sources: self.request.sources.clone(),
                scope: self.request.scope.clone(),
                require_debug_info: false, // TODO: undo once pdb works in symbolic
            })
            .map_err(|e| e.context(SymCacheErrorKind::Mailbox).into())
            .and_then(|x| Ok(x.context(SymCacheErrorKind::Fetching)?));

        let path = path.to_owned();
        let threadpool = self.threadpool.clone();

        let result = object.and_then(move |object| {
            threadpool.spawn_handle(lazy(move || {
                let file = BufWriter::new(File::create(&path).context(SymCacheErrorKind::Io)?);
                let object_inner = object.get_object().context(SymCacheErrorKind::Parse)?;
                let _file = symcache::SymCacheWriter::write_object(&object_inner, file)
                    .context(SymCacheErrorKind::Io)?;
                Ok(object.scope().clone())
            }))
        });

        Box::new(result)
    }

    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error> {
        Ok(SymCache {
            request: self,
            scope,
            inner: if !data.is_empty() { Some(data) } else { None },
        })
    }
}

/// Information for fetching the symbols for this symcache
#[derive(Debug, Clone)]
pub struct FetchSymCache {
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
    pub scope: Scope,
}

impl Message for FetchSymCache {
    type Result = Result<Arc<SymCache>, Arc<SymCacheError>>;
}

impl Handler<FetchSymCache> for SymCacheActor {
    type Result = ResponseFuture<Arc<SymCache>, Arc<SymCacheError>>;

    fn handle(&mut self, request: FetchSymCache, _ctx: &mut Self::Context) -> Self::Result {
        Box::new(
            self.symcaches
                .send(ComputeMemoized(FetchSymCacheInternal {
                    request,
                    objects: self.objects.clone(),
                    threadpool: self.threadpool.clone(),
                }))
                .map_err(|e| Arc::new(e.context(SymCacheErrorKind::Mailbox).into()))
                .and_then(|response| Ok(response?)),
        )
    }
}
