use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;
use std::fs::File;
use std::iter::FromIterator;
use std::panic;
use std::sync::Arc;
use std::time::Duration;

use actix::{
    fut::WrapFuture, Actor, ActorFuture, Addr, AsyncContext, Context, Handler, Message,
    ResponseActFuture,
};
use futures::future::{self, join_all, Either, Future, IntoFuture, Shared, SharedError};
use futures::sync::oneshot;
use sentry::integrations::failure::capture_fail;
use symbolic::common::{join_path, ByteView, InstructionInfo, Language};

use symbolic::demangle::{Demangle, DemangleFormat, DemangleOptions};
use symbolic::minidump::processor::{CodeModule, FrameInfoMap, ProcessState, RegVal};
use tokio::prelude::FutureExt;
use tokio_threadpool::ThreadPool;
use uuid;

use crate::actors::cficaches::{CfiCacheActor, CfiCacheError, CfiCacheErrorKind, FetchCfiCache};
use crate::actors::symcaches::{
    FetchSymCache, SymCacheActor, SymCacheError, SymCacheErrorKind, SymCacheFile,
};
use crate::hex::HexValue;
use crate::logging::LogError;
use crate::sentry::SentryFutureExt;
use crate::types::{
    ArcFail, CompleteObjectInfo, CompleteStacktrace, CompletedSymbolicationResponse, FrameStatus,
    ObjectFileStatus, ObjectId, ObjectType, RawFrame, RawObjectInfo, RawStacktrace, RequestId,
    Scope, Signal, SourceConfig, SymbolicatedFrame, SymbolicationError, SymbolicationResponse,
    SystemInfo,
};

const DEMANGLE_OPTIONS: DemangleOptions = DemangleOptions {
    with_arguments: true,
    format: DemangleFormat::Short,
};

// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<Result<Arc<T>, Arc<E>>>>;

pub struct SymbolicationActor {
    symcaches: Addr<SymCacheActor>,
    cficaches: Addr<CfiCacheActor>,
    threadpool: Arc<ThreadPool>,
    requests:
        BTreeMap<RequestId, ComputationChannel<CompletedSymbolicationResponse, SymbolicationError>>,
}

impl Actor for SymbolicationActor {
    type Context = Context<SymbolicationActor>;
}

impl SymbolicationActor {
    pub fn new(
        symcaches: Addr<SymCacheActor>,
        cficaches: Addr<CfiCacheActor>,
        threadpool: Arc<ThreadPool>,
    ) -> Self {
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
        channel: ComputationChannel<CompletedSymbolicationResponse, SymbolicationError>,
    ) -> ResponseActFuture<Self, SymbolicationResponse, SymbolicationError> {
        let rv = channel
            .map_err(|_: SharedError<oneshot::Canceled>| {
                panic!("Oneshot channel cancelled! Race condition or system shutting down")
            })
            .and_then(|result| (*result).clone())
            .map(|x| (*x).clone())
            .map_err(|e| {
                capture_fail(&*e);
                SymbolicationError::Mailbox
            });

        if let Some(timeout) = timeout {
            Box::new(
                rv.timeout(Duration::from_secs(timeout))
                    .into_actor(self)
                    .then(move |result, slf, _ctx| {
                        match result {
                            Ok(x) => {
                                slf.requests.remove(&request_id);
                                Ok(SymbolicationResponse::Completed(x))
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
                result
                    .map(SymbolicationResponse::Completed)
                    .into_future()
                    .into_actor(slf)
            }))
        }
    }

    fn do_symbolicate(
        &self,
        request: SymbolicateStacktraces,
    ) -> impl Future<Item = CompletedSymbolicationResponse, Error = SymbolicationError> {
        let signal = request.signal;
        let stacktraces = request.stacktraces.clone();

        let symcache_lookup: SymCacheLookup = request.modules.iter().cloned().collect();

        let threadpool = self.threadpool.clone();

        let result = symcache_lookup
            .fetch_symcaches(self.symcaches.clone(), request)
            .and_then(move |object_lookup| {
                threadpool.spawn_handle(
                    future::lazy(move || {
                        let stacktraces = stacktraces
                            .into_iter()
                            .map(|thread| symbolize_thread(thread, &object_lookup, signal))
                            .collect();

                        let modules = object_lookup
                            .inner
                            .into_iter()
                            .map(|(object_info, _)| object_info)
                            .collect();

                        Ok(CompletedSymbolicationResponse {
                            signal,
                            modules,
                            stacktraces,
                            ..Default::default()
                        })
                    })
                    .sentry_hub_current(),
                )
            });

        future_metrics!(
            "symbolicate",
            Some((Duration::from_secs(3600), SymbolicationError::Timeout)),
            result
        )
    }

    fn do_stackwalk_minidump(
        &self,
        request: ProcessMinidump,
    ) -> impl Future<Item = (SymbolicateStacktraces, MinidumpState), Error = SymbolicationError>
    {
        let ProcessMinidump {
            file,
            scope,
            sources,
        } = request;

        let byteview = tryf!(ByteView::map_file(file).map_err(|_| SymbolicationError::Minidump));

        let cfi_to_fetch = self.threadpool.spawn_handle(
            future::lazy(move || {
                log::debug!("Minidump size: {}", byteview.len());
                metric!(time_raw("minidump.upload.size") = byteview.len() as u64);
                let state = ProcessState::from_minidump(&byteview, None)
                    .map_err(|_| SymbolicationError::Minidump)?;

                let os_name = state.system_info().os_name();

                let cfi_to_fetch: Vec<_> = state
                    .referenced_modules()
                    .into_iter()
                    .filter_map(|code_module| {
                        Some((
                            code_module.id()?,
                            object_info_from_minidump_module(&os_name, code_module),
                        ))
                    })
                    .collect();

                Ok((byteview, cfi_to_fetch))
            })
            .sentry_hub_current(),
        );

        let cficaches = &self.cficaches;

        let cfi_requests = cfi_to_fetch.and_then(clone!(cficaches, scope, sources, |(
            byteview,
            object_infos,
        )| {
            join_all(
                object_infos
                    .into_iter()
                    .map(move |(code_module_id, object_info)| {
                        cficaches
                            .send(
                                FetchCfiCache {
                                    object_type: object_info.ty.clone(),
                                    identifier: object_id_from_object_info(&object_info),
                                    sources: sources.clone(),
                                    scope: scope.clone(),
                                }
                                .sentry_hub_new_from_current(),
                            )
                            .map_err(|_| SymbolicationError::Mailbox)
                            .map(move |result| (code_module_id, result))
                            // Clone hub because of join_all
                            .sentry_hub_new_from_current()
                    }),
            )
            .map(move |cfi_requests| (byteview, cfi_requests))
        }));

        let threadpool = &self.threadpool;

        let symbolication_request =
            cfi_requests.and_then(clone!(threadpool, scope, sources, |(
                byteview,
                cfi_requests,
            )| {
                threadpool.spawn_handle(
                    future::lazy(move || {
                        let mut frame_info_map = FrameInfoMap::new();
                        let mut unwind_statuses = BTreeMap::new();

                        for (code_module_id, result) in &cfi_requests {
                            let cache_file = match result {
                                Ok(x) => x,
                                Err(e) => {
                                    log::info!(
                                        "Error while fetching CFI cache: {}",
                                        LogError(&ArcFail(e.clone()))
                                    );
                                    unwind_statuses.insert(code_module_id, (&**e).into());
                                    continue;
                                }
                            };

                            let cfi_cache = match cache_file.parse() {
                                Ok(Some(x)) => x,
                                Ok(None) => {
                                    unwind_statuses
                                        .insert(code_module_id, ObjectFileStatus::Missing);
                                    continue;
                                }
                                Err(e) => {
                                    log::warn!("Error while parsing CFI cache: {}", LogError(&e));
                                    unwind_statuses.insert(code_module_id, (&e).into());
                                    continue;
                                }
                            };

                            unwind_statuses.insert(code_module_id, ObjectFileStatus::Found);
                            frame_info_map.insert(code_module_id.clone(), cfi_cache);
                        }

                        let process_state =
                            ProcessState::from_minidump(&byteview, Some(&frame_info_map))
                                .map_err(|_| SymbolicationError::Minidump)?;

                        let minidump_system_info = process_state.system_info();
                        let os_name = minidump_system_info.os_name();
                        let os_version = minidump_system_info.os_version();
                        let os_build = minidump_system_info.os_build();
                        let cpu_arch = minidump_system_info.cpu_arch();

                        let modules = process_state
                            .modules()
                            .into_iter()
                            .map(|code_module| {
                                let mut info: CompleteObjectInfo =
                                    object_info_from_minidump_module(&os_name, code_module).into();
                                info.unwind_status = Some(
                                    code_module
                                        .id()
                                        .and_then(|id| unwind_statuses.get(&id))
                                        .cloned()
                                        .unwrap_or(ObjectFileStatus::Unused),
                                );
                                info
                            })
                            .collect();

                        // This type only exists because ProcessState is not Send
                        let minidump_state = MinidumpState {
                            system_info: SystemInfo {
                                os_name,
                                os_version,
                                os_build,
                                cpu_arch,
                            },
                            requesting_thread_index: process_state
                                .requesting_thread()
                                .try_into()
                                .ok(),
                            crashed: process_state.crashed(),
                            crash_reason: process_state.crash_reason(),
                            assertion: process_state.assertion(),
                        };

                        let stacktraces = process_state
                            .threads()
                            .iter()
                            .map(|thread| {
                                let frames = thread.frames();
                                RawStacktrace {
                                    registers: frames
                                        .get(0)
                                        .map(|frame| {
                                            symbolic_registers_to_protocol_registers(
                                                &frame.registers(cpu_arch),
                                            )
                                        })
                                        .unwrap_or_default(),
                                    frames: frames
                                        .iter()
                                        .map(|frame| RawFrame {
                                            instruction_addr: HexValue(
                                                frame.return_address(cpu_arch),
                                            ),
                                            package: frame.module().map(CodeModule::code_file),
                                        })
                                        .collect(),
                                }
                            })
                            .collect();

                        Ok((
                            SymbolicateStacktraces {
                                modules,
                                scope,
                                sources,
                                signal: None,
                                stacktraces,
                            },
                            minidump_state,
                        ))
                    })
                    .sentry_hub_new_from_current(),
                )
            }));

        Box::new(future_metrics!(
            "minidump_stackwalk",
            Some((Duration::from_secs(1200), SymbolicationError::Timeout)),
            symbolication_request,
        ))
    }

    fn do_process_minidump(
        &self,
        request: ProcessMinidump,
    ) -> impl ActorFuture<Item = CompletedSymbolicationResponse, Error = SymbolicationError, Actor = Self>
    {
        let symbolication_request = self.do_stackwalk_minidump(request);

        let result = symbolication_request.into_actor(self).and_then(
            |(request, minidump_state), slf, _ctx| {
                slf.do_symbolicate(request)
                    .into_actor(slf)
                    .and_then(|mut response, slf, _ctx| {
                        let MinidumpState {
                            requesting_thread_index,
                            system_info,
                            crashed,
                            crash_reason,
                            assertion,
                        } = minidump_state;

                        response.system_info = Some(system_info);
                        response.crashed = Some(crashed);
                        response.crash_reason = Some(crash_reason);
                        response.assertion = Some(assertion);

                        for (i, mut stacktrace) in response.stacktraces.iter_mut().enumerate() {
                            stacktrace.is_requesting = requesting_thread_index.map(|r| r == i);
                        }

                        Ok(response).into_future().into_actor(slf)
                    })
            },
        );

        Box::new(result)
    }
}

fn object_id_from_object_info(object_info: &RawObjectInfo) -> ObjectId {
    ObjectId {
        debug_id: object_info.debug_id.as_ref().and_then(|x| x.parse().ok()),
        code_id: object_info.code_id.as_ref().and_then(|x| x.parse().ok()),
        debug_file: object_info.debug_file.clone(),
        code_file: object_info.code_file.clone(),
    }
}

fn get_image_type_from_minidump(minidump_os_name: &str) -> &'static str {
    match minidump_os_name {
        "Windows" | "Windows NT" => "pe",
        "iOS" | "Mac OS X" => "macho",
        "Linux" | "Solaris" | "Android" => "elf",
        _ => "unknown",
    }
}

fn object_info_from_minidump_module(minidump_os_name: &str, module: &CodeModule) -> RawObjectInfo {
    // TODO: should we also add `module.id()` somewhere?
    RawObjectInfo {
        ty: ObjectType(get_image_type_from_minidump(minidump_os_name).to_owned()),
        code_id: Some(module.code_identifier()),
        code_file: Some(module.code_file()),
        debug_id: Some(module.debug_identifier()),
        debug_file: Some(module.debug_file()),
        image_addr: HexValue(module.base_address()),
        image_size: if module.size() != 0 {
            Some(module.size())
        } else {
            None
        },
    }
}

struct SymCacheLookup {
    inner: Vec<(CompleteObjectInfo, Option<Arc<SymCacheFile>>)>,
}

impl FromIterator<CompleteObjectInfo> for SymCacheLookup {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = CompleteObjectInfo>,
    {
        let mut rv = SymCacheLookup {
            inner: iter.into_iter().map(|x| (x, None)).collect(),
        };
        rv.sort();
        rv
    }
}

impl SymCacheLookup {
    fn sort(&mut self) {
        self.inner.sort_by_key(|(info, _)| info.raw.image_addr.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        self.inner.dedup_by(|(ref info2, _), (ref mut info1, _)| {
            // If this underflows we didn't sort properly.
            let size = info2.raw.image_addr.0 - info1.raw.image_addr.0;
            info1.raw.image_size.get_or_insert(size);

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

        future::join_all(self.inner.into_iter().enumerate().map(
            move |(i, (mut object_info, _))| {
                if !referenced_objects.contains(&i) {
                    object_info.debug_status = ObjectFileStatus::Unused;
                    return Either::B(Ok((object_info, None)).into_future());
                }

                Either::A(
                    symcache_actor
                        .send(
                            FetchSymCache {
                                object_type: object_info.raw.ty.clone(),
                                identifier: object_id_from_object_info(&object_info.raw),
                                sources: sources.clone(),
                                scope: scope.clone(),
                            }
                            .sentry_hub_new_from_current(),
                        )
                        .map_err(|_| SymbolicationError::Mailbox)
                        .and_then(|result| {
                            result
                                .and_then(|symcache| match symcache.parse()? {
                                    Some(_) => Ok((Some(symcache), ObjectFileStatus::Found)),
                                    None => Ok((Some(symcache), ObjectFileStatus::Missing)),
                                })
                                .or_else(|e| Ok((None, (&*e).into())))
                        })
                        .map(move |(symcache, status)| {
                            object_info.arch =
                                symcache.as_ref().map(|c| c.arch()).unwrap_or_default();

                            object_info.debug_status = status;
                            (object_info, symcache)
                        }),
                )
            },
        ))
        .map(|results| SymCacheLookup {
            inner: results.into_iter().collect(),
        })
    }

    fn lookup_symcache(
        &self,
        addr: u64,
    ) -> Option<(usize, &CompleteObjectInfo, Option<&SymCacheFile>)> {
        for (i, (info, cache)) in self.inner.iter().enumerate() {
            let addr_smaller_than_end = if let Some(size) = info.raw.image_size {
                if let Some(end) = info.raw.image_addr.0.checked_add(size) {
                    addr < end
                } else {
                    // Image end addr is larger than u64::max, therefore addr (also u64) is smaller
                    // for sure.
                    true
                }
            } else {
                // The `size` is None (last image in array) and so we can't ensure it is within the
                // range.
                continue;
            };

            if info.raw.image_addr.0 <= addr && addr_smaller_than_end {
                return Some((i, info, cache.as_ref().map(|x| &**x)));
            }
        }

        None
    }
}

fn symbolize_thread(
    thread: RawStacktrace,
    caches: &SymCacheLookup,
    signal: Option<Signal>,
) -> CompleteStacktrace {
    let registers = thread.registers;

    let mut stacktrace = CompleteStacktrace {
        frames: vec![],
        ..Default::default()
    };

    let symbolize_frame = |i, frame: &RawFrame| -> Result<Vec<SymbolicatedFrame>, FrameStatus> {
        let (object_info, symcache) = match caches.lookup_symcache(frame.instruction_addr.0) {
            Some((_, info, Some(symcache))) => (info, symcache),
            Some((_, info, None)) => {
                if info.debug_status == ObjectFileStatus::Malformed {
                    return Err(FrameStatus::Malformed);
                } else {
                    return Err(FrameStatus::Missing);
                }
            }
            None => return Err(FrameStatus::UnknownImage),
        };

        let symcache = match symcache.parse() {
            Ok(Some(x)) => x,
            Ok(None) => return Err(FrameStatus::Missing),
            Err(_) => return Err(FrameStatus::Malformed),
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

        let relative_addr = match caller_address.checked_sub(object_info.raw.image_addr.0) {
            Some(x) => x,
            None => {
                log::warn!(
                    "Underflow when trying to subtract image start addr from caller address"
                );
                metric!(counter("relative_addr.underflow") += 1);
                return Err(FrameStatus::MissingSymbol);
            }
        };

        let line_infos = match symcache.lookup(relative_addr) {
            Ok(x) => x,
            Err(_) => return Err(FrameStatus::Malformed),
        };

        let mut rv = vec![];

        for line_info in line_infos {
            let line_info = match line_info {
                Ok(x) => x,
                Err(_) => return Err(FrameStatus::Malformed),
            };

            // The logic for filename and abs_path intentionally diverges from how symbolic is used
            // inside of Sentry right now.
            let (filename, abs_path) = {
                let comp_dir = line_info.compilation_dir();
                let rel_path = line_info.path();
                let abs_path = join_path(&comp_dir, &rel_path);

                if abs_path == rel_path {
                    // rel_path is absolute and therefore not usable for `filename`. Use the
                    // basename as filename.
                    (line_info.filename().to_owned(), abs_path.to_owned())
                } else {
                    // rel_path is relative (probably to the compilation dir) and therefore useful
                    // as filename for Sentry.
                    (rel_path.to_owned(), abs_path.to_owned())
                }
            };

            let lang = line_info.language();

            rv.push(SymbolicatedFrame {
                status: FrameStatus::Symbolicated,
                symbol: Some(line_info.symbol().to_string()),
                package: object_info.raw.code_file.clone(),
                abs_path: if !abs_path.is_empty() {
                    Some(abs_path)
                } else {
                    None
                },
                function: Some(
                    line_info
                        .function_name()
                        .try_demangle(DEMANGLE_OPTIONS)
                        .into_owned(),
                ),
                filename: if !filename.is_empty() {
                    Some(filename)
                } else {
                    None
                },
                lineno: Some(line_info.line()),
                instruction_addr: HexValue(
                    object_info.raw.image_addr.0 + line_info.instruction_address(),
                ),
                sym_addr: Some(HexValue(
                    object_info.raw.image_addr.0 + line_info.function_address(),
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
            rv.push(SymbolicatedFrame {
                status: FrameStatus::MissingSymbol,
                original_index: Some(i),
                instruction_addr: frame.instruction_addr,
                package: object_info.raw.code_file.clone(),
                ..Default::default()
            });
        }

        Ok(rv)
    };

    for (i, frame) in thread.frames.into_iter().enumerate() {
        match symbolize_frame(i, &frame) {
            Ok(frames) => stacktrace.frames.extend(frames),
            Err(status) => {
                stacktrace.frames.push(SymbolicatedFrame {
                    status,
                    original_index: Some(i),
                    instruction_addr: frame.instruction_addr,
                    ..Default::default()
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
    pub sources: Arc<Vec<SourceConfig>>,

    /// A list of threads containing stack traces.
    pub stacktraces: Vec<RawStacktrace>,

    /// A list of images that were loaded into the process.
    ///
    /// This list must cover the instruction addresses of the frames in `threads`. If a frame is not
    /// covered by any image, the frame cannot be symbolicated as it is not clear which debug file
    /// to load.
    pub modules: Vec<CompleteObjectInfo>,
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
                // Clone hub because of `ctx.spawn`
                .sentry_hub_new_from_current()
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
    pub sources: Arc<Vec<SourceConfig>>,
}

struct MinidumpState {
    system_info: SystemInfo,
    crashed: bool,
    crash_reason: String,
    assertion: String,
    requesting_thread_index: Option<usize>,
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
                .sentry_hub_new_from_current()
                .then(move |result, slf, _ctx| {
                    tx.send(match result {
                        Ok(x) => Ok(Arc::new(x)),
                        Err(e) => Err(Arc::new(e)),
                    })
                    .map_err(|_| ())
                    .into_future()
                    .into_actor(slf)
                }),
        );

        let channel = rx.shared();
        self.requests.insert(request_id.clone(), channel);

        Ok(request_id)
    }
}

fn symbolic_registers_to_protocol_registers(
    x: &BTreeMap<&'_ str, RegVal>,
) -> BTreeMap<String, HexValue> {
    x.iter()
        .map(|(&k, &v)| {
            (
                k.to_owned(),
                HexValue(match v {
                    RegVal::U32(x) => x.into(),
                    RegVal::U64(x) => x,
                }),
            )
        })
        .collect()
}

impl From<&CfiCacheError> for ObjectFileStatus {
    fn from(e: &CfiCacheError) -> ObjectFileStatus {
        match e.kind() {
            CfiCacheErrorKind::Fetching => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            CfiCacheErrorKind::Timeout => ObjectFileStatus::Timeout,
            CfiCacheErrorKind::ObjectParsing => ObjectFileStatus::Malformed,

            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_fail` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheErrorKind variant.
                capture_fail(e);
                ObjectFileStatus::Other
            }
        }
    }
}

impl From<&SymCacheError> for ObjectFileStatus {
    fn from(e: &SymCacheError) -> ObjectFileStatus {
        match e.kind() {
            SymCacheErrorKind::Fetching => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            SymCacheErrorKind::Timeout => ObjectFileStatus::Timeout,
            SymCacheErrorKind::ObjectParsing => ObjectFileStatus::Malformed,
            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_fail` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheErrorKind variant.
                capture_fail(e);
                ObjectFileStatus::Other
            }
        }
    }
}

handle_sentry_actix_message!(SymbolicationActor, GetSymbolicationStatus);
handle_sentry_actix_message!(SymbolicationActor, SymbolicateStacktraces);
handle_sentry_actix_message!(SymbolicationActor, ProcessMinidump);
