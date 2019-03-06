use crate::{
    actors::symcaches::{FetchSymCache, SymCache, SymCacheActor},
    log::LogError,
    types::{
        ErrorResponse, Frame, Meta, ObjectInfo, Stacktrace, SymbolicateFramesRequest,
        SymbolicateFramesResponse, SymbolicationError, Thread,
    },
};
use std::{iter::FromIterator, sync::Arc};
use void::Void;

use actix::{fut::WrapFuture, Actor, Addr, Context, Handler, ResponseActFuture};

use futures::future::{join_all, Future, IntoFuture};

use symbolic::common::{split_path, InstructionInfo};

use crate::actors::objects::ObjectId;

pub struct SymbolicationActor {
    symcaches: Addr<SymCacheActor>,
}

impl Actor for SymbolicationActor {
    type Context = Context<SymbolicationActor>;
}

impl SymbolicationActor {
    pub fn new(symcaches: Addr<SymCacheActor>) -> Self {
        SymbolicationActor { symcaches }
    }
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
        let symcaches = self.symcaches.clone();
        let threads = request.threads;

        let symcaches = join_all(request.modules.into_iter().map(move |object_info| {
            symcaches
                .send(FetchSymCache {
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
                })
                .map_err(SymbolicationError::from)
                .and_then(|result| Ok(result?))
                .then(move |result| Ok((object_info, result)))
                .map_err(|_: Void| unreachable!())
        }));

        let caches_map = symcaches.and_then(move |symcaches| {
            let mut errors = vec![];

            let caches_map = symcaches
                .into_iter()
                .filter_map(|(object_info, cache)| match cache {
                    Ok(x) => Some((object_info, x)),
                    Err(e) => {
                        log::debug!("Error while getting symcache: {}", LogError(&e));
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
