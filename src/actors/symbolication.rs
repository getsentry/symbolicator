use std::fs::File;
use std::io;

use actix::fut::{wrap_future, Either, WrapFuture};
use actix::Actor;
use actix::ActorFuture;
use actix::Addr;
use actix::Context;
use actix::Handler;
use actix::MailboxError;
use actix::Message;
use actix::ResponseActFuture;

use failure::Fail;

use futures::future::join_all;
use futures::future::{Future, IntoFuture};

use symbolic::common::split_path;
use symbolic::common::ByteView;
use symbolic::symcache;

use serde::Deserialize;
use serde::Deserializer;

use crate::actors::cache::CacheActor;
use crate::actors::cache::CacheItem;
use crate::actors::cache::Compute;
use crate::actors::cache::ComputeMemoized;
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
    #[fail(display = "Failed to fetch objects: {}", _0)]
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
                    .map(|x| {
                        let rv = x.has_debug_info();
                        println!("debug file has debug info: {}", rv);
                        rv
                    })
                    .unwrap_or(false)
                {
                    println!("Selecting debug file");
                    Either::A(Ok(debug_symbol).into_future().into_actor(slf))
                } else if code_symbol_inner
                    .map(|x| x.has_debug_info())
                    .unwrap_or(false)
                {
                    println!("Selecting code file");
                    Either::B(Either::A(Ok(code_symbol).into_future().into_actor(slf)))
                } else {
                    println!("Downloading breakpad file");
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

#[derive(Serialize, Deserialize)]
pub struct Frame {}

#[derive(Deserialize)]
pub struct ObjectInfo {
    debug_id: String,
    code_id: String,

    #[serde(default)]
    debug_name: Option<String>,

    #[serde(default)]
    code_name: Option<String>,
    //address: HexValue,
    //size: u64,

    //#[serde(default)]
    //module: Option<String>,

    //#[serde(default)]
    //name: Option<String>,

    //#[serde(default)]
    //symbol: Option<String>,

    //#[serde(default)]
    //symbol_address: Option<HexValue>,

    //#[serde(default)]
    //file: Option<String>,

    //#[serde(default)]
    //line: Option<u64>,

    //#[serde(default)]
    //line_address: Option<HexValue>,
}

struct HexValue(u64);

impl<'de> Deserialize<'de> for HexValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let string: &str = Deserialize::deserialize(deserializer)?;
        if string.starts_with("0x") || string.starts_with("0X") {
            if let Ok(x) = u64::from_str_radix(&string[2..], 16) {
                return Ok(HexValue(x));
            }
        }

        Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(string),
            &"a hex string starting with 0x",
        ))
    }
}

#[derive(Deserialize)]
pub struct SymbolicateFramesRequest {
    sources: Vec<SourceConfig>,
    frames: Vec<Frame>,
    modules: Vec<ObjectInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct ErrorResponse {}

#[derive(Serialize)]
#[serde(tag = "status")]
pub enum SymbolicateFramesResponse {
    //Pending {
    //retry_after: usize,
    //},
    Completed {
        frames: Vec<Frame>,
        errors: Vec<ErrorResponse>,
    },
}

impl Message for SymbolicateFramesRequest {
    type Result = Result<SymbolicateFramesResponse, SymbolicationError>;
}

impl Handler<SymbolicateFramesRequest> for SymbolicationActor {
    type Result = ResponseActFuture<Self, SymbolicateFramesResponse, SymbolicationError>;

    fn handle(
        &mut self,
        request: SymbolicateFramesRequest,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        let sources = request.sources;
        let objects = self.objects.clone();
        let symcache = self.symcache.clone();

        let symcaches = join_all(request.modules.into_iter().map(move |debug_info| {
            symcache
                .send(ComputeMemoized(SymCache {
                    inner: None,
                    identifier: ObjectId {
                        debug_id: debug_info.debug_id.parse().ok(),
                        code_id: Some(debug_info.code_id),
                        debug_name: debug_info
                            .debug_name
                            .as_ref()
                            .map(|x| split_path(x).1.to_owned()), // TODO
                        code_name: debug_info
                            .code_name
                            .as_ref()
                            .map(|x| split_path(x).1.to_owned()), // TODO
                    },
                    sources: sources.clone(),
                    objects: objects.clone(),
                }))
                .map_err(SymbolicationError::from)
                .flatten()
        }));

        let result = symcaches
            .and_then(|_| {
                Ok(SymbolicateFramesResponse::Completed {
                    frames: vec![],
                    errors: vec![],
                })
            })
            .into_actor(self);

        Box::new(result)
    }
}
