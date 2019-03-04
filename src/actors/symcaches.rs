use crate::actors::{
    cache::{CacheActor, CacheItem, Compute, ComputeMemoized, GetCacheKey, LoadCache},
    objects::{FetchObject, FileType, ObjectError, ObjectId, ObjectsActor, SourceConfig},
};
use actix::{
    fut::{wrap_future, ActorFuture, Either, WrapFuture},
    Actor, Addr, Context, Handler, MailboxError, Message, MessageResult, ResponseActFuture,
    ResponseFuture,
};
use failure::Fail;
use futures::{future::Future, IntoFuture};
use std::{fs::File, io};

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
    symcaches: Addr<CacheActor<SymCache>>,
    objects: Addr<ObjectsActor>,
}

impl Actor for SymCacheActor {
    type Context = Context<SymCacheActor>;
}

impl SymCacheActor {
    pub fn new(symcaches: Addr<CacheActor<SymCache>>, objects: Addr<ObjectsActor>) -> Self {
        SymCacheActor { symcaches, objects }
    }
}

#[derive(Clone)]
pub struct SymCache {
    // TODO: Symbolicate from object directly, put object (or only its byteview) here then
    // TODO: cache symbolic symcache here
    inner: Option<ByteView<'static>>,

    /// Information for fetching the symbols for this symcache
    identifier: ObjectId,
    sources: Vec<SourceConfig>,

    objects: Addr<ObjectsActor>,
}

struct GetSymCache;

impl Message for GetSymCache {
    type Result = SymCache;
}

impl Handler<GetSymCache> for SymCache {
    type Result = MessageResult<GetSymCache>;

    fn handle(&mut self, _item: GetSymCache, _ctx: &mut Self::Context) -> Self::Result {
        // TODO: Internal Arc to make this less expensive
        MessageResult(self.clone())
    }
}

impl SymCache {
    pub fn get_symcache(&self) -> Result<symcache::SymCache<'_>, SymCacheError> {
        Ok(symcache::SymCache::parse(
            self.inner.as_ref().ok_or(SymCacheError::NotFound)?,
        )?)
    }
}

impl CacheItem for SymCache {
    type Error = SymCacheError;
}

impl Actor for SymCache {
    type Context = Context<Self>;
}

impl Handler<Compute<SymCache>> for SymCache {
    type Result = ResponseActFuture<Self, (), <SymCache as CacheItem>::Error>;

    fn handle(&mut self, item: Compute<SymCache>, _ctx: &mut Self::Context) -> Self::Result {
        let debug_symbol = self
            .objects
            .send(FetchObject {
                filetype: FileType::Debug,
                identifier: self.identifier.clone(),
                sources: self.sources.clone(),
            })
            .map_err(SymCacheError::from)
            .and_then(|x| x.into_future().map_err(SymCacheError::from));

        let code_symbol = self
            .objects
            .send(FetchObject {
                filetype: FileType::Code,
                identifier: self.identifier.clone(),
                sources: self.sources.clone(),
            })
            .map_err(SymCacheError::from)
            .and_then(|x| x.into_future().map_err(SymCacheError::from));

        let result = (debug_symbol, code_symbol)
            .into_future()
            .into_actor(self)
            .and_then(|(debug_symbol, code_symbol), slf, _ctx| {
                // TODO: Fall back to symbol table (go debug -> code -> breakpad again)
                let debug_symbol_inner = debug_symbol.get_object();
                let code_symbol_inner = code_symbol.get_object();

                if debug_symbol_inner
                    .map(|_x| true) // x.has_debug_info()) // XXX: undo once pdb works in symbolic
                    .unwrap_or(false)
                {
                    Either::A(Ok(debug_symbol).into_future().into_actor(slf))
                } else if code_symbol_inner
                    .map(|x| x.has_debug_info())
                    .unwrap_or(false)
                {
                    Either::B(Either::A(Ok(code_symbol).into_future().into_actor(slf)))
                } else {
                    Either::B(Either::B(
                        slf.objects
                            .send(FetchObject {
                                filetype: FileType::Breakpad,
                                identifier: slf.identifier.clone(),
                                sources: slf.sources.clone(),
                            })
                            .map_err(SymCacheError::from)
                            .and_then(|x| x.into_future().map_err(SymCacheError::from))
                            .into_actor(slf),
                    ))
                }
            })
            .and_then(move |object, _slf, _ctx| {
                // TODO: SyncArbiter
                let file = tryfa!(File::create(&item.path));
                let object_inner = tryfa!(object.get_object());
                let _file = tryfa!(symcache::SymCacheWriter::write_object(&object_inner, file));

                Box::new(Ok(()).into())
            });

        Box::new(result)
    }
}

impl Handler<GetCacheKey> for SymCache {
    type Result = String;

    fn handle(&mut self, _item: GetCacheKey, _ctx: &mut Self::Context) -> Self::Result {
        // TODO: replace with new caching key discussed with jauer
        self.identifier.get_cache_key()
    }
}

impl Handler<LoadCache<SymCache>> for SymCache {
    type Result = ResponseActFuture<Self, (), <SymCache as CacheItem>::Error>;

    fn handle(&mut self, item: LoadCache<SymCache>, _ctx: &mut Self::Context) -> Self::Result {
        self.inner = Some(item.value);
        let _symcache = tryfa!(self.get_symcache());
        Box::new(wrap_future(Ok(()).into_future()))
    }
}

pub struct FetchSymCache {
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
}

impl Message for FetchSymCache {
    type Result = Result<SymCache, SymCacheError>;
}

impl Handler<FetchSymCache> for SymCacheActor {
    type Result = ResponseFuture<SymCache, SymCacheError>;

    fn handle(&mut self, message: FetchSymCache, _ctx: &mut Self::Context) -> Self::Result {
        Box::new(
            self.symcaches
                .send(ComputeMemoized(SymCache {
                    inner: None,
                    identifier: message.identifier,
                    sources: message.sources,
                    objects: self.objects.clone(),
                }))
                .map_err(SymCacheError::from)
                .and_then(|response| response.map_err(SymCacheError::from))
                .and_then(|result| result.send(GetSymCache).map_err(SymCacheError::from))
                .map_err(SymCacheError::from),
        )
    }
}
