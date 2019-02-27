use std::io;

use futures::future::IntoFuture;

use actix::fut::wrap_future;
use actix::Actor;
use actix::Addr;
use actix::Context;
use actix::Handler;
use actix::MailboxError;
use actix::ResponseActFuture;

use failure::Fail;

use symbolic::symcache;

use crate::actors::cache::CacheActor;
use crate::actors::cache::CacheItem;
use crate::actors::cache::Compute;
use crate::actors::cache::GetCacheKey;
use crate::actors::cache::LoadCache;
use crate::actors::debugsymbols::DebugInfoError;
use crate::actors::debugsymbols::DebugInfoId;
use crate::actors::debugsymbols::DebugSymbolsActor;
use crate::actors::debugsymbols::FetchDebugInfo;
use crate::actors::debugsymbols::SourceConfig;

#[derive(Debug, Fail, From)]
pub enum SymbolicationError {
    #[fail(display = "Failed to fetch debug symbols: {}", _0)]
    Fetching(DebugInfoError),

    #[fail(display = "Failed sending message to actor: {}", _0)]
    Mailbox(MailboxError),

    #[fail(display = "Failed to download: {}", _0)]
    Io(io::Error),

    #[fail(display = "???")]
    Other,
}

pub struct SymbolicationActor {
    symcache: Addr<CacheActor<SymCache>>,
    debugsymbols: Addr<DebugSymbolsActor>,
}

impl Actor for SymbolicationActor {
    type Context = Context<SymbolicationActor>;
}

impl SymbolicationActor {
    pub fn new(
        symcache: Addr<CacheActor<SymCache>>,
        debugsymbols: Addr<DebugSymbolsActor>,
    ) -> Self {
        SymbolicationActor {
            symcache,
            debugsymbols,
        }
    }
}

pub struct SymCache {
    // TODO: Symbolicate from object.. sometimes
    inner: Option<symcache::SymCache<'static>>,

    /// Information for fetching the symbols for this symcache
    debug_info_request: FetchDebugInfo,

    debugsymbols: Addr<DebugSymbolsActor>,
}

impl SymCache {
    pub fn as_inner(&self) -> Option<&symcache::SymCache<'static>> {
        self.inner.as_ref()
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
        let debug_symbols = self.debugsymbols.send(self.debug_info_request.clone());
        unimplemented!();
    }
}

impl Handler<GetCacheKey> for SymCache {
    type Result = String;

    fn handle(&mut self, _item: GetCacheKey, _ctx: &mut Self::Context) -> Self::Result {
        // TODO: replace with new caching key discussed with jauer
        self.debug_info_request.identifier.get_cache_key()
    }
}

impl Handler<LoadCache<SymCache>> for SymCache {
    type Result = ResponseActFuture<Self, (), <SymCache as CacheItem>::Error>;

    fn handle(&mut self, item: LoadCache<SymCache>, _ctx: &mut Self::Context) -> Self::Result {
        unimplemented!();
        //Box::new(wrap_future(Ok(()).into_future()))
    }
}
