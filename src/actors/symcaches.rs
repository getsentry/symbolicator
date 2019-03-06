use crate::actors::{
    cache::{CacheActor, CacheItemRequest, CacheKey, ComputeMemoized, Scope},
    objects::{FetchObject, FileType, ObjectError, ObjectId, ObjectsActor, SourceConfig},
};
use actix::{Actor, Addr, Context, Handler, MailboxError, Message, ResponseFuture};
use failure::Fail;
use futures::{
    future::{Either, Future},
    IntoFuture,
};
use std::{fs::File, io, path::Path};

use symbolic::{common::ByteView, symcache};

#[derive(Fail, Debug, derive_more::From)]
pub enum SymCacheError {
    #[fail(display = "Failed to fetch objects: {}", _0)]
    Fetching(#[fail(cause)] ObjectError),

    #[fail(display = "Failed to download: {}", _0)]
    Io(#[fail(cause)] io::Error),

    #[fail(display = "Failed sending message to objects actor: {}", _0)]
    Mailbox(#[fail(cause)] MailboxError),

    #[fail(display = "Failed to parse symcache during download: {}", _0)]
    Parse(#[fail(cause)] symcache::SymCacheError),

    #[fail(display = "Symcache not found")]
    NotFound,
}

pub struct SymCacheActor {
    symcaches: Addr<CacheActor<FetchSymCacheInternal>>,
    objects: Addr<ObjectsActor>,
}

impl Actor for SymCacheActor {
    type Context = Context<SymCacheActor>;
}

impl SymCacheActor {
    pub fn new(
        symcaches: Addr<CacheActor<FetchSymCacheInternal>>,
        objects: Addr<ObjectsActor>,
    ) -> Self {
        SymCacheActor { symcaches, objects }
    }
}

#[derive(Debug, Clone)]
pub struct SymCache {
    inner: Option<ByteView<'static>>,
    scope: Scope,
    request: FetchSymCacheInternal,
}

impl SymCache {
    pub fn get_symcache(&self) -> Result<symcache::SymCache<'_>, SymCacheError> {
        Ok(symcache::SymCache::parse(
            self.inner.as_ref().ok_or(SymCacheError::NotFound)?,
        )?)
    }
}

#[derive(Debug, Clone)]
pub struct FetchSymCacheInternal {
    request: FetchSymCache,
    objects: Addr<ObjectsActor>,
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCache;
    type Error = SymCacheError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.request.identifier.get_cache_key(),
            scope: Scope::Scoped("fakeproject".to_owned()), // TODO: Write project id here
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let objects = self.objects.clone();

        let debug_symbol = objects
            .send(FetchObject {
                filetype: FileType::Debug,
                identifier: self.request.identifier.clone(),
                sources: self.request.sources.clone(),
            })
            .map_err(SymCacheError::from)
            .and_then(|x| Ok(x?));

        let code_symbol = objects
            .send(FetchObject {
                filetype: FileType::Code,
                identifier: self.request.identifier.clone(),
                sources: self.request.sources.clone(),
            })
            .map_err(SymCacheError::from)
            .and_then(|x| Ok(x?));

        let breakpad_request = FetchObject {
            filetype: FileType::Breakpad,
            identifier: self.request.identifier.clone(),
            sources: self.request.sources.clone(),
        };

        let path = path.to_owned();

        let result = (debug_symbol, code_symbol)
            .into_future()
            .and_then(move |(debug_symbol, code_symbol)| {
                // TODO: Fall back to symbol table (go debug -> code -> breakpad again)
                let debug_symbol_inner = debug_symbol.get_object();
                let code_symbol_inner = code_symbol.get_object();

                if debug_symbol_inner
                    .map(|_x| true) // x.has_debug_info()) // XXX: undo once pdb works in symbolic
                    .unwrap_or(false)
                {
                    Either::A(Ok(debug_symbol).into_future())
                } else if code_symbol_inner
                    .map(|x| x.has_debug_info())
                    .unwrap_or(false)
                {
                    Either::A(Ok(code_symbol).into_future())
                } else {
                    Either::B(
                        objects
                            .send(breakpad_request)
                            .map_err(SymCacheError::from)
                            .and_then(|x| Ok(x?)),
                    )
                }
            })
            .and_then(move |object| {
                // TODO: SyncArbiter
                let file = File::create(&path).unwrap();
                let object_inner = object.get_object().unwrap();
                let _file = symcache::SymCacheWriter::write_object(&object_inner, file).unwrap();

                // TODO: Use scope of object
                Box::new(Ok(object.scope().clone()).into_future())
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
}

impl Message for FetchSymCache {
    type Result = Result<SymCache, SymCacheError>;
}

impl Handler<FetchSymCache> for SymCacheActor {
    type Result = ResponseFuture<SymCache, SymCacheError>;

    fn handle(&mut self, request: FetchSymCache, _ctx: &mut Self::Context) -> Self::Result {
        Box::new(
            self.symcaches
                .send(ComputeMemoized(FetchSymCacheInternal {
                    request,
                    objects: self.objects.clone(),
                }))
                .map_err(SymCacheError::from)
                .and_then(|response| Ok(response?)),
        )
    }
}
