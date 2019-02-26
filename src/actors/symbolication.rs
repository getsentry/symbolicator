//use actix::Context;
//use actix::Addr;

//use failure::Fail;

//use symbolic::symcache;

//use crate::actors::cache::CacheActor;
//use crate::actors::cache::CacheItem;
//use crate::actors::debugsymbols::DebugSymbolsActor;

//#[derive(Fail, Debug, From)]
//pub enum SymbolicationError {
//#[fail(display = "Failed to fetch debug symbols: {}", _0)]
//Fetching(DebugSymbolsActor),

//#[fail(display = "???")]
//Other,
//}

//pub struct SymbolicationActor {
//symcache: Addr<CacheActor<SymCache>>,
//debugsymbols: Addr<DebugSymbolsActor>
//}

//impl SymbolicationActor {
//pub fn new(symcache: Addr<CacheActor<SymCache>>, debugsymbols: Addr<DebugSymbolsActor>) -> Self {
//SymbolicationActor {
//symcache,
//debugsymbols
//}
//}
//}

//pub struct SymCache {
//inner: symcache::SymCache<'static>,
//// TODO: Need addr to SymbolicationActor
//// TODO: Need DebugInfoId
//}

//impl CacheItem for SymCache {
//type Error = SymbolicationError;
//}

//impl Actor for SymCache {
//type Context = Context<Self>;
//}

//impl Handler<Compute<SymCache>> for SymCache {
//type Result = ResponseActFuture<Self, (), <SymCache as CacheItem>::Error>;

//fn handle(&mut self, item: Compute<SymCache>, _ctx: &mut Self::Context) -> Self::Result {
//let debug_symbols = self.debugsymbols.send(FetchDebugInfo {
//})
//}
//}
