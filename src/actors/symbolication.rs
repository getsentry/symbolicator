use std::{collections::BTreeMap, iter::FromIterator, sync::Arc, time::Duration};

use actix::{
    fut::WrapFuture, Actor, ActorFuture, Addr, AsyncContext, Context, Handler, ResponseActFuture,
};

use failure::{Fail, ResultExt};

use futures::future::{join_all, lazy, Future};

use futures::{
    future::{IntoFuture, Shared, SharedError},
    sync::oneshot,
};

use symbolic::common::{split_path, InstructionInfo};

use tokio::prelude::FutureExt;

use tokio_threadpool::ThreadPool;

use uuid;

use void::Void;

use crate::futures::measure_task;

use crate::{
    actors::symcaches::{FetchSymCache, SymCache, SymCacheActor},
    log::LogError,
    types::{
        ArcFail, ErrorResponse, Frame, Meta, ObjectId, ObjectInfo, Stacktrace,
        SymbolicateFramesRequest, SymbolicateFramesResponse, SymbolicationError,
        SymbolicationErrorKind, Thread,
    },
};

// Inner result necessary because `futures::Shared` won't give us `Arc`s but its own custom
// newtype around it.
type ComputationChannel<T, E> = Shared<oneshot::Receiver<Result<Arc<T>, Arc<E>>>>;

pub struct SymbolicationActor {
    symcaches: Addr<SymCacheActor>,
    threadpool: Arc<ThreadPool>,
    requests: BTreeMap<String, ComputationChannel<SymbolicateFramesResponse, SymbolicationError>>,
}

impl Actor for SymbolicationActor {
    type Context = Context<SymbolicationActor>;
}

impl SymbolicationActor {
    pub fn new(symcaches: Addr<SymCacheActor>, threadpool: Arc<ThreadPool>) -> Self {
        let requests = BTreeMap::new();

        SymbolicationActor {
            symcaches,
            threadpool,
            requests,
        }
    }

    fn do_symbolicate(
        &mut self,
        request: SymbolicateFramesRequest,
    ) -> impl Future<Item = SymbolicateFramesResponse, Error = SymbolicationError> {
        let sources = request.sources;
        let meta = request.meta;
        let scope = meta.scope.clone();
        let symcaches = self.symcaches.clone();
        let threads = request.threads;

        let symcaches = join_all(request.modules.into_iter().map(move |object_info| {
            symcaches
                .send(FetchSymCache {
                    object_type: object_info.ty.clone(),
                    identifier: ObjectId {
                        debug_id: object_info.debug_id.parse().ok(),
                        code_id: object_info.code_id.as_ref().and_then(|x| x.parse().ok()),
                        debug_name: object_info
                            .debug_file
                            .as_ref()
                            .map(|x| split_path(x).1.to_owned()), // TODO
                        code_name: object_info
                            .code_file
                            .as_ref()
                            .map(|x| split_path(x).1.to_owned()), // TODO
                    },
                    sources: sources.clone(),
                    scope: scope.clone(),
                })
                .map_err(|e| e.context(SymbolicationErrorKind::Mailbox).into())
                // XXX: `result.context` should work
                .and_then(|result| {
                    result.map_err(|e| {
                        SymbolicationError::from(
                            ArcFail(e).context(SymbolicationErrorKind::Caching),
                        )
                    })
                })
                .then(move |result| Ok((object_info, result)))
                .map_err(|_: Void| unreachable!())
        }));

        let threadpool = self.threadpool.clone();

        let result = symcaches.and_then(move |symcaches| {
            threadpool.spawn_handle(lazy(move || {
                let mut errors = vec![];

                let symcache_map = symcaches
                    .into_iter()
                    .filter_map(|(object_info, cache)| match cache {
                        Ok(x) => Some((object_info, x)),
                        Err(e) => {
                            log::debug!("Error while getting symcache: {}", LogError(&e));
                            errors.push(ErrorResponse::NativeMissingDsym {
                                // TODO: Proper error types
                                data: LogError(&e).to_string(),
                            });
                            None
                        }
                    })
                    .collect::<SymCacheMap>();

                let stacktraces = threads
                    .into_iter()
                    .map(|thread| symbolize_thread(thread, &symcache_map, &meta, &mut errors))
                    .collect();

                Ok(SymbolicateFramesResponse::Completed {
                    stacktraces,
                    errors,
                })
            }))
        });

        measure_task(
            "symbolicate",
            Some((Duration::from_secs(420), || {
                SymbolicationErrorKind::Timeout.into()
            })),
            result,
        )
    }
}

impl Handler<SymbolicateFramesRequest> for SymbolicationActor {
    type Result = ResponseActFuture<Self, SymbolicateFramesResponse, SymbolicationError>;

    fn handle(
        &mut self,
        mut request: SymbolicateFramesRequest,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        if let Some(request_meta) = request.request.take() {
            let request_id = request_meta.request_id.clone().unwrap_or_else(|| loop {
                let request_id = uuid::Uuid::new_v4().to_string();
                if !self.requests.contains_key(&request_id) {
                    break request_id;
                }
            });

            let channel = if let Some(channel) = self.requests.get(&request_id) {
                channel.clone()
            } else {
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
                self.requests.insert(request_id.clone(), channel.clone());
                channel
            };

            Box::new(
                channel
                    .map_err(|_: SharedError<oneshot::Canceled>| {
                        panic!("Oneshot channel cancelled! Race condition or system shutting down")
                    })
                    .and_then(|result| (*result).clone())
                    .map(|x| (*x).clone())
                    .map_err(|e| ArcFail(e).context(SymbolicationErrorKind::Mailbox).into())
                    .timeout(Duration::from_secs(request_meta.timeout))
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
                                    Ok(SymbolicateFramesResponse::Pending {
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
            Box::new(self.do_symbolicate(request).into_actor(self))
        }
    }
}

struct SymCacheMap {
    inner: Vec<(ObjectInfo, Arc<SymCache>)>,
}

impl FromIterator<(ObjectInfo, Arc<SymCache>)> for SymCacheMap {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (ObjectInfo, Arc<SymCache>)>,
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
        self.inner.sort_by_key(|(info, _)| info.image_addr.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        self.inner.dedup_by(|(ref info2, _), (ref mut info1, _)| {
            info1
                .image_size
                .get_or_insert(info2.image_addr.0 - info1.image_addr.0);
            false
        });
    }

    fn lookup_symcache(&self, addr: u64) -> Option<(&ObjectInfo, &SymCache)> {
        for (ref info, ref cache) in self.inner.iter().peekable() {
            // When `size` is None, this must be the last item.
            if info.image_addr.0 <= addr && addr <= info.image_addr.0 + info.image_size? {
                return Some((info, cache));
            }
        }

        None
    }
}

fn symbolize_thread(
    thread: Thread,
    caches: &SymCacheMap,
    meta: &Meta,
    errors: &mut Vec<ErrorResponse>,
) -> Stacktrace {
    let ip_reg = if let Some(ip_reg_name) = meta.arch.ip_register_name() {
        Some(thread.registers.get(ip_reg_name).map(|x| x.0))
    } else {
        None
    };

    let mut stacktrace = Stacktrace { frames: vec![] };

    let symbolize_frame =
        |stacktrace: &mut Stacktrace, i, frame: &Frame| -> Result<(), SymbolicationError> {
            let caller_address = if let Some(ip_reg) = ip_reg {
                let instruction = InstructionInfo {
                    addr: frame.instruction_addr.0,
                    arch: meta.arch,
                    signal: meta.signal,
                    crashing_frame: i == 0,
                    ip_reg,
                };
                instruction.caller_address()
            } else {
                frame.instruction_addr.0
            };

            let (symcache_info, symcache) = caches
                .lookup_symcache(caller_address)
                .ok_or(SymbolicationErrorKind::SymCacheNotFound)?;
            let symcache = symcache
                .get_symcache()
                .context(SymbolicationErrorKind::SymCache)?;

            let mut had_frames = false;

            for line_info in symcache
                .lookup(caller_address - symcache_info.image_addr.0)
                .context(SymbolicationErrorKind::SymCache)?
            {
                let line_info = line_info.context(SymbolicationErrorKind::SymCache)?;
                had_frames = true;

                // TODO(jauer): Verify, this was a hackjob
                stacktrace.frames.push(Frame {
                    symbol: Some(line_info.symbol().to_string()),
                    abs_path: Some(line_info.path()),
                    package: symcache_info.code_file.clone(),
                    function: Some(line_info.function_name().to_string()), // TODO: demangle
                    filename: Some(line_info.filename().to_string()), // TODO: Relative path to compilation_dir
                    ..frame.clone()
                });
            }

            if had_frames {
                Ok(())
            } else {
                Err(SymbolicationErrorKind::NotFound.into())
            }
        };

    for (i, mut frame) in thread.stacktrace.frames.into_iter().enumerate() {
        let addr = frame.instruction_addr;
        frame.original_index = Some(i);

        let res = symbolize_frame(&mut stacktrace, i, &frame);
        if let Err(e) = res {
            stacktrace.frames.push(frame);
            errors.push(ErrorResponse::NativeMissingDsym {
                // TODO: Proper error types
                data: format!("Failed to symbolicate addr {}: {}", addr, LogError(&e)),
            });
        };
    }

    stacktrace
}
