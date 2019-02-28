use std::fs::File;
use std::io;

use actix::fut::{wrap_future, Either, WrapFuture};
use actix::Actor;
use actix::ActorFuture;
use actix::Addr;
use actix::Context;
use actix::Handler;
use actix::MailboxError;
use actix::ResponseActFuture;

use failure::Fail;

use futures::future::{Future, IntoFuture};

use symbolic::common::ByteView;
use symbolic::symcache;

use crate::actors::cache::CacheActor;
use crate::actors::cache::CacheItem;
use crate::actors::cache::Compute;
use crate::actors::cache::GetCacheKey;
use crate::actors::cache::LoadCache;
use crate::actors::objects::FetchObject;
use crate::actors::objects::FileType;
use crate::actors::objects::ObjectError;
use crate::actors::objects::ObjectId;
use crate::actors::objects::ObjectsActor;
use crate::actors::objects::SourceConfig;

#[derive(Debug, Fail, From)]
pub enum SymbolicationError {
    #[fail(display = "Failed to fetch debug symbols: {}", _0)]
    Fetching(ObjectError),

    #[fail(display = "Failed sending message to actor: {}", _0)]
    Mailbox(MailboxError),

    #[fail(display = "Failed to download: {}", _0)]
    Io(io::Error),

    #[fail(display = "Failed to parse symcache: {}", _0)]
    Parse(symcache::SymCacheError),

    #[fail(display = "Symcache not found")]
    NotFound,
}

pub struct SymbolicationActor {
    symcache: Addr<CacheActor<SymCache>>,
    objects: Addr<ObjectsActor>,
}

impl Actor for SymbolicationActor {
    type Context = Context<SymbolicationActor>;
}

impl SymbolicationActor {
    pub fn new(symcache: Addr<CacheActor<SymCache>>, objects: Addr<ObjectsActor>) -> Self {
        SymbolicationActor { symcache, objects }
    }
}

pub struct SymCache {
    // TODO: Symbolicate from object directly, put object (or only its byteview) here then
    inner: Option<ByteView<'static>>,

    /// Information for fetching the symbols for this symcache
    identifier: ObjectId,
    sources: Vec<SourceConfig>,

    objects: Addr<ObjectsActor>,
}

impl SymCache {
    fn get_symcache<'a>(&'a self) -> Result<symcache::SymCache<'a>, SymbolicationError> {
        Ok(symcache::SymCache::parse(
            self.inner.as_ref().ok_or(SymbolicationError::NotFound)?,
        )?)
    }
}

impl CacheItem for SymCache {
    type Error = SymbolicationError;
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
            .map_err(SymbolicationError::from)
            .and_then(|x| x.into_future().map_err(SymbolicationError::from));

        let code_symbol = self
            .objects
            .send(FetchObject {
                filetype: FileType::Code,
                identifier: self.identifier.clone(),
                sources: self.sources.clone(),
            })
            .map_err(SymbolicationError::from)
            .and_then(|x| x.into_future().map_err(SymbolicationError::from));

        let result = (debug_symbol, code_symbol)
            .into_future()
            .into_actor(self)
            .and_then(|(debug_symbol, code_symbol), slf, _ctx| {
                let debug_symbol_inner = debug_symbol.get_object();
                let code_symbol_inner = code_symbol.get_object();

                if debug_symbol_inner
                    .map(|x| x.has_debug_info())
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
                            .map_err(SymbolicationError::from)
                            .and_then(|x| x.into_future().map_err(SymbolicationError::from))
                            .into_actor(slf),
                    ))
                }
            })
            .and_then(move |object, _slf, _ctx| {
                // TODO: SyncArbiter
                let file = tryfa!(File::open(&item.path));
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
