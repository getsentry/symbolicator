use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};
use bytes::Bytes;
use failure::{Fail, ResultExt};

use futures::future::{self, Either};
use futures::{Future, IntoFuture, Stream};
use symbolic::common::ByteView;
use symbolic::debuginfo::Object;
use tokio_threadpool::ThreadPool;

use crate::actors::cache::{CacheActor, CacheItemRequest, CacheKey};
use crate::futures::measure_task;
use crate::types::{FileType, ObjectId, Scope, SourceConfig};

mod http;
mod paths;
mod s3;
mod sentry;

const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Fail, Clone, Copy)]
pub enum ObjectErrorKind {
    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "failed sending message to actor")]
    Mailbox,

    #[fail(display = "failed parsing object")]
    Parsing,

    #[fail(display = "mismatching IDs")]
    IdMismatch,

    #[fail(display = "bad status code")]
    BadStatusCode,

    #[fail(display = "failed sending request to source")]
    SendRequest,

    #[fail(display = "failed to look into cache")]
    Caching,

    #[fail(display = "object download took too long")]
    Timeout,

    #[fail(display = "failed fetching data from Sentry")]
    Sentry,
}

symbolic::common::derive_failure!(
    ObjectError,
    ObjectErrorKind,
    doc = "Errors happening while fetching objects"
);

impl From<io::Error> for ObjectError {
    fn from(e: io::Error) -> Self {
        e.context(ObjectErrorKind::Io).into()
    }
}

#[derive(Debug, Clone)]
pub struct FetchFile {
    scope: Scope,
    file_id: FileId,
    source: SourceConfig,
    threadpool: Arc<ThreadPool>,
}

impl CacheItemRequest for FetchFile {
    type Item = ObjectFile;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: match self.file_id {
                FileId::External {
                    ref object_id,
                    filetype,
                    ..
                } => format!(
                    "{}.{}.{}",
                    self.source.id(),
                    object_id.cache_key(),
                    filetype.as_ref()
                ),
                FileId::Sentry { ref sentry_id, .. } => {
                    format!("{}.{}.sentryinternal", self.source.id(), sentry_id)
                }
            },
            scope: self.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let request = download_from_source(&self.source, &self.file_id);
        let path = path.to_owned();
        let source = self.source.clone();
        let request_scope = self.scope.clone();
        let threadpool = self.threadpool.clone();

        let final_scope = if source.is_public() {
            Scope::Global
        } else {
            request_scope
        };

        let mut cache_key = self.get_cache_key();
        cache_key.scope = final_scope.clone();

        let result = request.and_then(move |payload| {
            if let Some(payload) = payload {
                log::info!("Resolved debug file for {}", cache_key);

                let future = fs::File::create(&path)
                    .map_err(|e| e.context(ObjectErrorKind::Io).into())
                    .into_future()
                    .and_then(|file| {
                        payload.fold(file, move |mut file, chunk| {
                            threadpool.spawn_handle(future::lazy(move || {
                                file.write_all(&chunk).map(|_| file)
                            }))
                        })
                    })
                    .and_then(|file| {
                        // Ensure that both meta data and file contents are available to the
                        // subsequent reads of the file metadata and reads from other threads.
                        file.sync_all().context(ObjectErrorKind::Io)?;

                        let metadata = file.metadata().context(ObjectErrorKind::Io)?;
                        metric!(gauge("objects.size") = metadata.len());

                        Ok(final_scope)
                    });

                Either::A(future)
            } else {
                log::debug!("No debug file found for {}", cache_key);
                Either::B(Ok(final_scope).into_future())
            }
        });

        Box::new(measure_task(
            "objects",
            Some((Duration::from_secs(600), ObjectErrorKind::Timeout.into())),
            result,
        ))
    }

    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error> {
        Ok(ObjectFile {
            request: Some(self),
            scope,
            object: if data.is_empty() { None } else { Some(data) },
        })
    }
}

/// Handle to local cache file.
#[derive(Debug, Clone)]
pub struct ObjectFile {
    request: Option<FetchFile>,
    scope: Scope,
    object: Option<ByteView<'static>>,
}

impl ObjectFile {
    pub fn parse(&self) -> Result<Option<Object<'_>>, ObjectError> {
        let bytes = match self.object {
            Some(ref x) => x,
            None => return Ok(None),
        };
        let parsed = Object::parse(&bytes).context(ObjectErrorKind::Parsing)?;

        if let Some(ref request) = self.request {
            let object_id = match request.file_id {
                FileId::External { ref object_id, .. } => object_id,
                FileId::Sentry { ref object_id, .. } => object_id,
            };

            if let Some(ref debug_id) = object_id.debug_id {
                let parsed_id = parsed.debug_id();

                // Microsoft symbol server sometimes stores updated files with a more recent
                // (=higher) age, but resolves it for requests with lower ages as well. Thus, we
                // need to check whether the parsed debug file fullfills the *miniumum* age bound.
                // For example:
                // `4A236F6A0B3941D1966B41A4FC77738C2` is reported as
                // `4A236F6A0B3941D1966B41A4FC77738C4` from the server.
                //                                  ^
                if parsed_id.uuid() != debug_id.uuid() || parsed_id.appendix() < debug_id.appendix()
                {
                    metric!(counter("object.debug_id_mismatch") += 1);
                    log::debug!(
                        "debug id mismatch. got {}, expected {}",
                        parsed.debug_id(),
                        debug_id
                    );
                    return Err(ObjectErrorKind::IdMismatch.into());
                }
            }

            if let Some(ref code_id) = object_id.code_id {
                if let Some(ref object_code_id) = parsed.code_id() {
                    if object_code_id != code_id {
                        metric!(counter("object.code_id_mismatch") += 1);
                        log::debug!(
                            "code id mismatch. got {}, expected {}",
                            object_code_id,
                            code_id
                        );
                        return Err(ObjectErrorKind::IdMismatch.into());
                    }
                }
            }
        }

        Ok(Some(parsed))
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }
}

#[derive(Clone)]
pub struct ObjectsActor {
    cache: Addr<CacheActor<FetchFile>>,
    threadpool: Arc<ThreadPool>,
}

impl ObjectsActor {
    pub fn new(cache: Addr<CacheActor<FetchFile>>, threadpool: Arc<ThreadPool>) -> Self {
        ObjectsActor { cache, threadpool }
    }
}

impl Actor for ObjectsActor {
    type Context = Context<Self>;
}

/// Fetch a Object from external sources or internal cache.
#[derive(Debug, Clone)]
pub struct FetchObject {
    pub filetypes: &'static [FileType],
    pub scope: Scope,
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
}

impl Message for FetchObject {
    type Result = Result<Arc<ObjectFile>, ObjectError>;
}

impl Handler<FetchObject> for ObjectsActor {
    type Result = ResponseFuture<Arc<ObjectFile>, ObjectError>;

    fn handle(&mut self, request: FetchObject, _ctx: &mut Self::Context) -> Self::Result {
        let FetchObject {
            filetypes,
            scope,
            identifier,
            sources,
        } = request;

        let prepare_futures: Vec<_> = sources
            .iter()
            .map(|source| {
                prepare_downloads(
                    source,
                    scope.clone(),
                    filetypes,
                    &identifier,
                    self.threadpool.clone(),
                    self.cache.clone(),
                )
            })
            .collect();

        Box::new(
            future::join_all(prepare_futures).and_then(move |responses| {
                responses
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .min_by_key(|(ref i, response)| {
                        (
                            // Prefer object files with debug info over object files without
                            // Prefer files that contain an object over unparseable files
                            match response
                                .as_ref()
                                .ok()
                                .and_then(|o| Some(o.parse().ok()??.has_debug_info()))
                            {
                                Some(true) => 0,
                                Some(false) => 1,
                                None => 2,
                            },
                            *i,
                        )
                    })
                    .map(|(_, response)| response)
                    .unwrap_or_else(move || {
                        Ok(Arc::new(ObjectFile {
                            request: None,
                            scope,
                            object: None,
                        }))
                    })
            }),
        )
    }
}

type PrioritizedDownloads = Vec<Result<Arc<ObjectFile>, ObjectError>>;
type DownloadStream = Box<dyn Stream<Item = Bytes, Error = ObjectError>>;

#[derive(Debug, Clone)]
pub enum FileId {
    External {
        filetype: FileType,
        object_id: ObjectId,
    },
    Sentry {
        object_id: ObjectId,
        sentry_id: String,
    },
}

fn prepare_downloads(
    source: &SourceConfig,
    scope: Scope,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
    threadpool: Arc<ThreadPool>,
    cache: Addr<CacheActor<FetchFile>>,
) -> Box<Future<Item = PrioritizedDownloads, Error = ObjectError>> {
    match *source {
        SourceConfig::Sentry(ref source) => {
            sentry::prepare_downloads(source, scope, filetypes, object_id, threadpool, cache)
        }
        SourceConfig::Http(ref source) => {
            http::prepare_downloads(source, scope, filetypes, object_id, threadpool, cache)
        }
        SourceConfig::S3(ref source) => {
            s3::prepare_downloads(source, scope, filetypes, object_id, threadpool, cache)
        }
    }
}

fn download_from_source(
    source: &SourceConfig,
    file_id: &FileId,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    match *source {
        SourceConfig::Sentry(ref x) => sentry::download_from_source(x, file_id),
        SourceConfig::Http(ref x) => http::download_from_source(x, file_id),
        SourceConfig::S3(ref x) => s3::download_from_source(x, file_id),
    }
}
