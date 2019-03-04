use crate::log::LogError;
use actix::MessageResult;
use serde::{Serialize, Serializer};
use std::{collections::BTreeMap, fmt, fs::File, io, iter::FromIterator, sync::Arc};

use actix::{
    fut::{wrap_future, Either, WrapFuture},
    Actor, ActorFuture, Addr, Context, Handler, MailboxError, Message, ResponseActFuture,
};

use failure::Fail;

use futures::future::{join_all, Future, IntoFuture};

use symbolic::{
    common::{split_path, Arch, ByteView, InstructionInfo},
    symcache,
};

use serde::{Deserialize, Deserializer};

use crate::actors::{
    cache::{CacheActor, CacheItem, Compute, ComputeMemoized, GetCacheKey, LoadCache},
    objects::{FetchObject, FileType, ObjectError, ObjectId, ObjectsActor, SourceConfig},
};

#[derive(Debug, Fail, From)]
pub enum SymbolicationError {
    #[fail(display = "Failed to fetch objects: {}", _0)]
    Fetching(#[fail(cause)] ObjectError),

    #[fail(display = "Failed sending message to actor: {}", _0)]
    Mailbox(#[fail(cause)] MailboxError),

    #[fail(display = "Failed to download: {}", _0)]
    Io(#[fail(cause)] io::Error),

    #[fail(display = "Failed to parse symcache: {}", _0)]
    Parse(#[fail(cause)] symcache::SymCacheError),

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
                // TODO: Fall back to symbol table (go debug -> code -> breakpad again)
                let debug_symbol_inner = debug_symbol.get_object();
                let code_symbol_inner = code_symbol.get_object();

                if debug_symbol_inner
                    .map(|_x| true) // x.has_debug_info()) // XXX: undo
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

#[derive(Clone, Serialize, Deserialize)]
pub struct Frame {
    addr: HexValue,

    module: Option<String>, // NOTE: This is "package" in Sentry
    name: Option<String>,   // Only present if ?demangle was set
    language: Option<String>,
    symbol: Option<String>,
    symbol_address: Option<HexValue>,
    file: Option<String>,
    line: Option<u64>,
    line_address: Option<HexValue>, // NOTE: This does not exist in Sentry
}

#[derive(Deserialize)]
pub struct ObjectInfo {
    debug_id: String,
    code_id: Option<String>,

    #[serde(default)]
    debug_name: Option<String>,

    #[serde(default)]
    code_name: Option<String>,

    address: HexValue,

    #[serde(default)]
    size: Option<u64>,
}

#[derive(Clone, Debug, Copy)]
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

impl<'d> fmt::Display for HexValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl Serialize for HexValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

#[derive(Deserialize)]
pub struct SymbolicateFramesRequest {
    #[serde(default)]
    meta: Meta,
    #[serde(default)]
    sources: Vec<SourceConfig>,
    #[serde(default)]
    threads: Vec<Thread>,
    #[serde(default)]
    modules: Vec<ObjectInfo>,
}

#[derive(Clone, Deserialize, Default)]
pub struct Meta {
    #[serde(default)]
    signal: Option<u32>,
    #[serde(default)]
    arch: Arch,
}

#[derive(Deserialize)]
pub struct Thread {
    registers: BTreeMap<String, HexValue>,
    #[serde(flatten)]
    stacktrace: Stacktrace,
}

#[derive(Serialize, Deserialize)]
pub struct Stacktrace {
    frames: Vec<Frame>,
}

#[derive(Serialize, Deserialize)]
pub struct ErrorResponse(String);

#[derive(Serialize)]
#[serde(tag = "status")]
pub enum SymbolicateFramesResponse {
    //Pending {
    //retry_after: usize,
    //},
    Completed {
        stacktraces: Vec<Stacktrace>,
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
        let meta = request.meta;
        let objects = self.objects.clone();
        let symcache = self.symcache.clone();
        let threads = request.threads;

        let symcaches = join_all(request.modules.into_iter().map(move |object_info| {
            symcache
                .send(ComputeMemoized(SymCache {
                    inner: None,
                    identifier: ObjectId {
                        debug_id: object_info.debug_id.parse().ok(),
                        code_id: object_info.code_id.clone(),
                        debug_name: object_info
                            .debug_name
                            .as_ref()
                            .map(|x| split_path(x).1.to_owned()), // TODO
                        code_name: object_info
                            .code_name
                            .as_ref()
                            .map(|x| split_path(x).1.to_owned()), // TODO
                    },
                    sources: sources.clone(),
                    objects: objects.clone(),
                }))
                .map_err(SymbolicationError::from)
                .flatten()
                .and_then(|cache| cache.send(GetSymCache).map_err(SymbolicationError::from))
                .then(move |result| Ok((object_info, result)))
                .map_err(|_: ()| unreachable!())
        }));

        let caches_map = symcaches.and_then(move |symcaches| {
            let mut errors = vec![];

            let caches_map = symcaches
                .into_iter()
                .filter_map(|(object_info, cache)| match cache {
                    Ok(x) => Some((object_info, x)),
                    Err(e) => {
                        debug!("Error while getting symcache: {}", LogError(&e));
                        errors.push(ErrorResponse(format!("{}", e)));
                        None
                    }
                })
                .collect::<SymCacheMap>();

            Ok((errors, Arc::new(caches_map)))
        });

        let result = caches_map
            .and_then(move |(mut errors, caches_map)| {
                join_all(threads.into_iter().map(move |thread| {
                    // TODO: SyncArbiter
                    Ok(symbolize_thread(thread, caches_map.clone(), meta.clone())).into_future()
                }))
                .and_then(move |stacktraces_chunks| {
                    let mut stacktraces = vec![];

                    for (stacktrace, errors_chunk) in stacktraces_chunks {
                        stacktraces.push(stacktrace);
                        errors.extend(errors_chunk);
                    }

                    Ok(SymbolicateFramesResponse::Completed {
                        stacktraces,
                        errors,
                    })
                    .into_future()
                })
            })
            .into_actor(self);

        Box::new(result)
    }
}

struct SymCacheMap {
    inner: Vec<(ObjectInfo, SymCache)>,
}

impl FromIterator<(ObjectInfo, SymCache)> for SymCacheMap {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (ObjectInfo, SymCache)>,
    {
        let mut rv = SymCacheMap {
            inner: iter.into_iter().collect(),
        };
        rv.sort();
        rv
    }
}

impl SymCacheMap {
    fn sort(&mut self) {
        self.inner.sort_by_key(|(info, _)| info.address.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        self.inner.dedup_by(|(ref info2, _), (ref mut info1, _)| {
            info1.size.get_or_insert(info2.address.0 - info1.address.0);
            false
        });
    }

    fn lookup_symcache(&self, addr: u64) -> Option<(&ObjectInfo, &SymCache)> {
        for (ref info, ref cache) in self.inner.iter().peekable() {
            // When `size` is None, this must be the last item.
            if info.address.0 <= addr && addr <= info.address.0 + info.size? {
                return Some((info, cache));
            }
        }

        None
    }
}

fn symbolize_thread(
    thread: Thread,
    caches: Arc<SymCacheMap>,
    meta: Meta,
) -> (Stacktrace, Vec<ErrorResponse>) {
    let ip_reg = if let Some(ip_reg_name) = meta.arch.ip_register_name() {
        Some(thread.registers.get(ip_reg_name).map(|x| x.0))
    } else {
        None
    };

    let mut stacktrace = Stacktrace { frames: vec![] };
    let mut errors = vec![];

    let symbolize_frame =
        |stacktrace: &mut Stacktrace, i, frame: &Frame| -> Result<(), SymbolicationError> {
            let caller_address = if let Some(ip_reg) = ip_reg {
                let instruction = InstructionInfo {
                    addr: frame.addr.0,
                    arch: meta.arch,
                    signal: meta.signal,
                    crashing_frame: i == 0,
                    ip_reg,
                };
                instruction.caller_address()
            } else {
                frame.addr.0
            };

            let (symcache_info, symcache) = caches
                .lookup_symcache(caller_address)
                .ok_or(SymbolicationError::NotFound)?;
            let symcache = symcache.get_symcache()?;

            let mut had_frames = false;

            for line_info in symcache.lookup(caller_address - symcache_info.address.0)? {
                let line_info = line_info?;
                had_frames = true;

                stacktrace.frames.push(Frame {
                    symbol: Some(line_info.symbol().to_string()),
                    name: Some(line_info.function_name().as_str().to_owned()), // TODO: demangle
                    ..frame.clone()
                });
            }

            if had_frames {
                Ok(())
            } else {
                Err(SymbolicationError::NotFound)
            }
        };

    for (i, frame) in thread.stacktrace.frames.into_iter().enumerate() {
        let addr = frame.addr;
        let res = symbolize_frame(&mut stacktrace, i, &frame);
        if let Err(e) = res {
            stacktrace.frames.push(frame);
            errors.push(ErrorResponse(format!(
                "Failed to symbolicate addr {}: {}",
                addr, e
            )));
        };
    }

    (stacktrace, errors)
}
