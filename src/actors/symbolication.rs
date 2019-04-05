use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::iter::FromIterator;
use std::sync::Arc;
use std::time::Duration;

use actix::{
    fut::WrapFuture, Actor, ActorFuture, Addr, AsyncContext, Context, Handler, Message,
    ResponseActFuture,
};
use failure::{Fail, ResultExt};
use futures::future::{self, Either, Future, IntoFuture, Shared, SharedError, join_all};
use futures::sync::oneshot;
use symbolic::common::{split_path, InstructionInfo, Language, ByteView};
use symbolic::minidump::processor::{CodeModule, ProcessState, FrameInfoMap};
use tokio::prelude::FutureExt;
use tokio_threadpool::ThreadPool;
use uuid;


use sentry::integrations::failure::capture_fail;

use crate::actors::symcaches::{FetchSymCache, SymCacheActor, SymCacheErrorKind, SymCacheFile};
use crate::actors::cficaches::{FetchCfiCache, CfiCacheActor, CfiCacheErrorKind, CfiCacheFile};
use crate::futures::measure_task;
use crate::logging::LogError;
use crate::types::{
    ArcFail, DebugFileStatus, FetchedDebugFile, FrameStatus, HexValue, ObjectId, ObjectInfo,
    RawFrame, RawStacktrace, RequestId, Scope, Signal, SourceConfig, SymbolicatedFrame,
    SymbolicatedStacktrace, SymbolicationError, SymbolicationErrorKind, SymbolicationResponse, ObjectType,
};


// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<Result<Arc<T>, Arc<E>>>>;

pub struct SymbolicationActor {
    symcaches: Addr<SymCacheActor>,
    cficaches: Addr<CfiCacheActor>,
    threadpool: Arc<ThreadPool>,
    requests: BTreeMap<RequestId, ComputationChannel<SymbolicationResponse, SymbolicationError>>,
}

impl Actor for SymbolicationActor {
    type Context = Context<SymbolicationActor>;
}

impl SymbolicationActor {
    pub fn new(symcaches: Addr<SymCacheActor>, cficaches: Addr<CfiCacheActor>, threadpool: Arc<ThreadPool>) -> Self {
        let requests = BTreeMap::new();

        SymbolicationActor {
            symcaches,
            cficaches,
            threadpool,
            requests,
        }
    }

    fn wrap_response_channel(
        &self,
        request_id: RequestId,
        timeout: Option<u64>,
        channel: ComputationChannel<SymbolicationResponse, SymbolicationError>,
    ) -> ResponseActFuture<Self, SymbolicationResponse, SymbolicationError> {
        let rv = channel
            .map_err(|_: SharedError<oneshot::Canceled>| {
                panic!("Oneshot channel cancelled! Race condition or system shutting down")
            })
            .and_then(|result| (*result).clone())
            .map(|x| (*x).clone())
            .map_err(|e| ArcFail(e).context(SymbolicationErrorKind::Mailbox).into());

        if let Some(timeout) = timeout {
            Box::new(
                rv.timeout(Duration::from_secs(timeout))
                    .into_actor(self)
                    .then(move |result, slf, _ctx| {
                        match result {
                            Ok(x) => {
                                slf.requests.remove(&request_id);
                                Ok(x)
                            }
                            Err(e) => {
                                if let Some(inner) = e.into_inner() {
                                    slf.requests.remove(&request_id);
                                    Err(inner)
                                } else {
                                    Ok(SymbolicationResponse::Pending {
                                        request_id,
                                        // XXX(markus): Probably need a better estimation at some
                                        // point.
                                        retry_after: 30,
                                    })
                                }
                            }
                        }
                        .into_future()
                        .into_actor(slf)
                    }),
            )
        } else {
            Box::new(rv.into_actor(self).then(move |result, slf, _ctx| {
                slf.requests.remove(&request_id);
                result.into_future().into_actor(slf)
            }))
        }
    }

    fn do_symbolicate(
        &mut self,
        request: SymbolicateStacktraces,
    ) -> impl Future<Item = SymbolicationResponse, Error = SymbolicationError> {
        let signal = request.signal;
        let stacktraces = request.stacktraces.clone();

        let object_lookup: SymCacheLookup = request.modules.iter().cloned().collect();

        let threadpool = self.threadpool.clone();

        let result = object_lookup
            .fetch_symcaches(self.symcaches.clone(), request)
            .and_then(move |object_lookup| {
                threadpool.spawn_handle(future::lazy(move || {
                    let stacktraces = stacktraces
                        .into_iter()
                        .map(|thread| symbolize_thread(thread, &object_lookup, signal))
                        .collect();

                    let modules = object_lookup
                        .inner
                        .into_iter()
                        .map(|(object_info, cache, status)| FetchedDebugFile {
                            status,
                            arch: cache.as_ref().map(|c| c.arch()).unwrap_or_default(),
                            object_info,
                        })
                        .collect();

                    Ok(SymbolicationResponse::Completed {
                        signal,
                        modules,
                        stacktraces,
                    })
                }))
            });

        measure_task(
            "symbolicate",
            Some((
                Duration::from_secs(3600),
                SymbolicationErrorKind::Timeout.into(),
            )),
            result,
        )
    }

    fn do_process_minidump(&mut self, request: ProcessMinidump) -> impl Future<Item = SymbolicationResponse, Error = SymbolicationError> {
        let ProcessMinidump { file, scope, sources }  =request;

        let byteview = tryf!(ByteView::read(file).context(SymbolicationErrorKind::Minidump));

        let object_infos = self.threadpool.spawn_handle(future::lazy(move || {
            // XXX: Use ByteView::from_file
            let state = ProcessState::from_minidump(&byteview, None)
                .context(SymbolicationErrorKind::Minidump)?;

            Ok((
                byteview,
                state.referenced_modules().into_iter()
                .filter_map(|code_module| Some((
                    code_module.id()?,
                    object_info_from_minidump_module(code_module))
                ))
                .collect::<Vec<_>>()
            ))
        }));

        let cficaches = self.cficaches.clone();

        let cfi_requests = object_infos.and_then(|(byteview, object_infos)| {
            join_all(object_infos.into_iter().map(move |(code_module_id, object_info)| {
                cficaches.send(FetchCfiCache {
                    object_type: object_info.ty.clone(),
                    identifier: object_id_from_object_info(&object_info),
                    sources: sources.clone(),
                    scope: scope.clone()
                })
                .map_err(|e| e.context(SymbolicationErrorKind::Mailbox))
                .map_err(SymbolicationError::from)
                .map(move |result| (object_info, code_module_id, result))
            }))
            .map(move |cfi_requests| (byteview, cfi_requests))
        });

        let symbolicator_request = cfi_requests.and_then(|(byteview, cfi_requests)| {
            self.threadpool.spawn_handle(future::lazy(move || {
                let mut frame_info_map = FrameInfoMap::new();
                let mut modules = vec![];

                for (object_info, code_module_id, result) in &cfi_requests {
                    // XXX: We should actually build a list of FetchedDebugFile instead of
                    // discarding errors.
                    let cache_file = match result {
                        Ok(x) => x,
                        Err(e) => {
                            log::info!("Error while fetching CFI cache: {}", LogError(&ArcFail(e.clone())));
                            continue;
                        }
                    };

                    modules.push(object_info.clone());

                    let cfi_cache = match cache_file.parse() {
                        Ok(Some(x)) => x,
                        Ok(None) => continue,
                        Err(e) => {
                            log::warn!("Error while parsing CFI cache: {}", LogError(&e));
                            continue;
                        }
                    };

                    frame_info_map.insert(code_module_id.clone(), cfi_cache);
                }

                let process_state = ProcessState::from_minidump(&byteview, Some(&frame_info_map)).context(SymbolicationErrorKind::Minidump)?;
                Ok(SymbolicateStacktraces {
                    modules: vec![],
                    scope: 
                })
            }))
        });

        Box::new(symbolicator_request.and_then(|_| Ok(loop {})))
    }
}

fn object_id_from_object_info(object_info: &ObjectInfo) -> ObjectId {
    ObjectId {
        debug_id: object_info.debug_id.as_ref().and_then(|x| x.parse().ok()),
        code_id: object_info.code_id.as_ref().and_then(|x| x.parse().ok()),
        debug_file: object_info
            .debug_file
            .as_ref()
            .map(|x| split_path(x).1.to_owned()),
        code_file: object_info
            .code_file
            .as_ref()
            .map(|x| split_path(x).1.to_owned()),
    }
}

fn object_info_from_minidump_module(module: &CodeModule) -> ObjectInfo {
    // TODO: should we also add `module.id()` somewhere?
    ObjectInfo {
        ty: ObjectType("unknown".to_owned()),  // TODO: read from system info?
        code_id: Some(module.code_identifier()),
        code_file: Some(split_path(&module.code_file()).1.to_owned()),
        debug_id: Some(module.debug_identifier()),
        debug_file: Some(split_path(&module.debug_file()).1.to_owned()),
        image_addr: HexValue(module.base_address()),
        image_size: if module.size() != 0 { Some(module.size()) } else { None },
    }
}

struct SymCacheLookup {
    inner: Vec<(ObjectInfo, Option<Arc<SymCacheFile>>, DebugFileStatus)>,
}

impl FromIterator<ObjectInfo> for SymCacheLookup {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = ObjectInfo>,
    {
        let mut rv = SymCacheLookup {
            inner: iter
                .into_iter()
                .map(|x| (x, None, DebugFileStatus::Unused))
                .collect(),
        };
        rv.sort();
        rv
    }
}

impl SymCacheLookup {
    fn sort(&mut self) {
        self.inner.sort_by_key(|(info, _, _)| info.image_addr.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        self.inner
            .dedup_by(|(ref info2, _, _), (ref mut info1, _, _)| {
                info1
                    .image_size
                    .get_or_insert(info2.image_addr.0 - info1.image_addr.0);
                false
            });
    }

    fn fetch_symcaches(
        self,
        symcache_actor: Addr<SymCacheActor>,
        request: SymbolicateStacktraces,
    ) -> impl Future<Item = Self, Error = SymbolicationError> {
        let mut referenced_objects = BTreeSet::new();
        let sources = request.sources;
        let stacktraces = request.stacktraces;
        let scope = request.scope.clone();

        for stacktrace in stacktraces {
            for frame in stacktrace.frames {
                if let Some((i, ..)) = self.lookup_symcache(frame.instruction_addr.0) {
                    referenced_objects.insert(i);
                }
            }
        }

        future::join_all(
            self.inner
                .into_iter()
                .enumerate()
                .map(move |(i, (object_info, _, _))| {
                    if !referenced_objects.contains(&i) {
                        return Either::B(
                            Ok((object_info, None, DebugFileStatus::Unused)).into_future(),
                        );
                    }

                    Either::A(
                        symcache_actor
                            .send(FetchSymCache {
                                object_type: object_info.ty.clone(),
                                identifier: object_id_from_object_info(&object_info),
                                sources: sources.clone(),
                                scope: scope.clone(),
                            })
                            .map_err(|e| e.context(SymbolicationErrorKind::Mailbox).into())
                            .and_then(move |result| {
                                let symcache = match result {
                                    Ok(x) => x,
                                    Err(e) => {
                                        let status = match e.kind() {
                                            SymCacheErrorKind::Fetching => {
                                                DebugFileStatus::FetchingFailed
                                            }
                                            SymCacheErrorKind::Timeout => {
                                                // Timeouts of object downloads are caught by
                                                // FetchingFailed
                                                DebugFileStatus::TooLarge
                                            }
                                            _ => {
                                                capture_fail(&ArcFail(e));
                                                DebugFileStatus::Other
                                            }
                                        };

                                        return Ok((object_info, None, status));
                                    }
                                };

                                let status = match symcache.parse() {
                                    Ok(Some(_)) => DebugFileStatus::Found,
                                    Ok(None) => DebugFileStatus::MissingDebugFile,
                                    Err(e) => match e.kind() {
                                        SymCacheErrorKind::ObjectParsing => {
                                            DebugFileStatus::MalformedDebugFile
                                        }
                                        _ => {
                                            capture_fail(&e);
                                            DebugFileStatus::Other
                                        }
                                    },
                                };

                                Ok((object_info, Some(symcache), status))
                            }),
                    )
                }),
        )
        .map(|results| SymCacheLookup {
            inner: results.into_iter().collect(),
        })
    }

    fn lookup_symcache(
        &self,
        addr: u64,
    ) -> Option<(usize, &ObjectInfo, Option<&SymCacheFile>, DebugFileStatus)> {
        for (i, (info, cache, status)) in self.inner.iter().enumerate() {
            // When `size` is None, this must be the last item.
            if info.image_addr.0 <= addr && addr <= info.image_addr.0 + info.image_size? {
                return Some((i, info, cache.as_ref().map(|x| &**x), *status));
            }
        }

        None
    }
}

fn symbolize_thread(
    thread: RawStacktrace,
    caches: &SymCacheLookup,
    signal: Option<Signal>,
) -> SymbolicatedStacktrace {
    let registers = thread.registers;

    let mut stacktrace = SymbolicatedStacktrace { frames: vec![] };

    let symbolize_frame = |i, frame: &RawFrame| -> Result<Vec<SymbolicatedFrame>, FrameStatus> {
        let (object_info, symcache) = match caches.lookup_symcache(frame.instruction_addr.0) {
            Some((_, symcache_info, Some(symcache), _)) => (symcache_info, symcache),
            Some((_, _, None, _)) => return Err(FrameStatus::MissingDebugFile),
            None => return Err(FrameStatus::UnknownImage),
        };

        let symcache = match symcache.parse() {
            Ok(Some(x)) => x,
            Ok(None) => return Err(FrameStatus::MissingDebugFile),
            Err(_) => return Err(FrameStatus::MalformedDebugFile),
        };

        let crashing_frame = i == 0;

        let ip_reg = if crashing_frame {
            symcache
                .arch()
                .ip_register_name()
                .and_then(|ip_reg_name| registers.get(ip_reg_name))
                .map(|x| x.0)
        } else {
            None
        };

        let instruction_info = InstructionInfo {
            addr: frame.instruction_addr.0,
            arch: symcache.arch(),
            signal: signal.map(|x| x.0),
            crashing_frame,
            ip_reg,
        };
        let caller_address = instruction_info.caller_address();

        let line_infos = match symcache.lookup(caller_address - object_info.image_addr.0) {
            Ok(x) => x,
            Err(_) => return Err(FrameStatus::MalformedDebugFile),
        };

        let mut rv = vec![];

        for line_info in line_infos {
            let line_info = match line_info {
                Ok(x) => x,
                Err(_) => return Err(FrameStatus::MalformedDebugFile),
            };

            let abs_path = line_info.path();
            let filename = line_info.filename().to_string(); // TODO: Relative path to compilation_dir
            let lang = line_info.language();
            rv.push(SymbolicatedFrame {
                status: FrameStatus::Symbolicated,
                symbol: Some(line_info.symbol().to_string()),
                abs_path: if !abs_path.is_empty() {
                    Some(abs_path)
                } else {
                    None
                },
                package: object_info.code_file.clone(),
                function: Some(line_info.function_name().to_string()), // TODO: demangle
                filename: if !filename.is_empty() {
                    Some(filename)
                } else {
                    None
                },
                lineno: Some(line_info.line()),
                instruction_addr: HexValue(
                    object_info.image_addr.0 + line_info.instruction_address(),
                ),
                sym_addr: Some(HexValue(
                    object_info.image_addr.0 + line_info.function_address(),
                )),
                lang: if lang != Language::Unknown {
                    Some(lang)
                } else {
                    None
                },
                original_index: Some(i),
            });
        }

        if rv.is_empty() {
            Err(FrameStatus::MissingSymbol)
        } else {
            Ok(rv)
        }
    };

    for (i, frame) in thread.frames.into_iter().enumerate() {
        match symbolize_frame(i, &frame) {
            Ok(frames) => stacktrace.frames.extend(frames),
            Err(status) => {
                stacktrace.frames.push(SymbolicatedFrame {
                    status,
                    original_index: Some(i),
                    instruction_addr: frame.instruction_addr,
                    package: None,
                    lang: None,
                    symbol: None,
                    function: None,
                    filename: None,
                    abs_path: None,
                    lineno: None,
                    sym_addr: None,
                });
            }
        }
    }

    stacktrace
}

/// A request for symbolication of multiple stack traces.
pub struct SymbolicateStacktraces {
    /// The scope of this request which determines access to cached files.
    pub scope: Scope,

    /// The signal thrown on certain operating systems.
    ///
    ///  Signal handlers sometimes mess with the runtime stack. This is used to determine whether
    /// the top frame should be fixed or not.
    pub signal: Option<Signal>,

    /// A list of external sources to load debug files.
    pub sources: Vec<SourceConfig>,

    /// A list of threads containing stack traces.
    pub stacktraces: Vec<RawStacktrace>,

    /// A list of images that were loaded into the process.
    ///
    /// This list must cover the instruction addresses of the frames in `threads`. If a frame is not
    /// covered by any image, the frame cannot be symbolicated as it is not clear which debug file
    /// to load.
    pub modules: Vec<ObjectInfo>,
}

impl Message for SymbolicateStacktraces {
    type Result = Result<RequestId, SymbolicationError>;
}

impl Handler<SymbolicateStacktraces> for SymbolicationActor {
    type Result = Result<RequestId, SymbolicationError>;

    fn handle(&mut self, request: SymbolicateStacktraces, ctx: &mut Self::Context) -> Self::Result {
        let request_id = loop {
            let request_id = RequestId(uuid::Uuid::new_v4().to_string());
            if !self.requests.contains_key(&request_id) {
                break request_id;
            }
        };

        let (tx, rx) = oneshot::channel();

        ctx.spawn(
            self.do_symbolicate(request)
                .then(move |result| {
                    tx.send(match result {
                        Ok(x) => Ok(Arc::new(x)),
                        Err(e) => Err(Arc::new(e)),
                    })
                    .map_err(|_| ())
                })
                .into_actor(self),
        );

        let channel = rx.shared();
        self.requests.insert(request_id.clone(), channel);

        Ok(request_id)
    }
}

/// Status poll request.
pub struct GetSymbolicationStatus {
    /// The identifier of the symbolication task.
    pub request_id: RequestId,
    /// An optional timeout, after which the request will yield a result.
    ///
    /// If this timeout is not set, symbolication will continue until a result is ready (which is
    /// either an error or success). If this timeout is set and no result is ready, a `pending`
    /// status is returned.
    pub timeout: Option<u64>,
}

impl Message for GetSymbolicationStatus {
    type Result = Result<Option<SymbolicationResponse>, SymbolicationError>;
}

impl Handler<GetSymbolicationStatus> for SymbolicationActor {
    type Result = ResponseActFuture<Self, Option<SymbolicationResponse>, SymbolicationError>;

    fn handle(
        &mut self,
        request: GetSymbolicationStatus,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        let request_id = request.request_id;

        if let Some(channel) = self.requests.get(&request_id) {
            Box::new(
                self.wrap_response_channel(request_id, request.timeout, channel.clone())
                    .map(|x, _, _| Some(x)),
            )
        } else {
            Box::new(Ok(None).into_future().into_actor(self))
        }
    }
}

/// A request for a minidump to be stackwalked and symbolicated. Internally this will basically be
/// converted into a `SymbolicateStacktraces`
pub struct ProcessMinidump {
    /// The scope of this request which determines access to cached files.
    pub scope: Scope,

    /// Handle to minidump file. The message handler should not worry about resource cleanup: The
    /// inode does not have a hardlink, so closing the file should clean up the tempfile.
    pub file: File,

    /// A list of external sources to load debug files.
    pub sources: Vec<SourceConfig>,
}

impl Message for ProcessMinidump {
    type Result = Result<RequestId, SymbolicationError>;
}

impl Handler<ProcessMinidump> for SymbolicationActor {
    type Result = Result<RequestId, SymbolicationError>;

    fn handle(&mut self, request: ProcessMinidump, ctx: &mut Self::Context) -> Self::Result {
        let request_id = loop {
            let request_id = RequestId(uuid::Uuid::new_v4().to_string());
            if !self.requests.contains_key(&request_id) {
                break request_id;
            }
        };

        let (tx, rx) = oneshot::channel();

        ctx.spawn(
            self.do_process_minidump(request)
            .then(move |result| {
                tx.send(match result {
                    Ok(x) => Ok(Arc::new(x)),
                    Err(e) => Err(Arc::new(e)),
                })
                .map_err(|_| ())
            })
            .into_actor(self)
        );

        let channel = rx.shared();
        self.requests.insert(request_id.clone(), channel);

        Ok(request_id)
    }
}
