use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, Addr, Context, Handler, Message, ResponseFuture};
use bytes::Bytes;
use failure::{Fail, ResultExt};

use futures::{future, Future, IntoFuture, Stream};
use symbolic::common::ByteView;
use symbolic::debuginfo::Object;
use tempfile::tempfile_in;
use tokio_threadpool::ThreadPool;

use crate::actors::cache::{CacheActor, CacheItemRequest, CacheKey};
use crate::types::{
    FileType, HttpSourceConfig, ObjectId, S3SourceConfig, Scope, SentrySourceConfig, SourceConfig,
};

mod http;
mod paths;
mod s3;
mod sentry;

const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Fail, Clone, Copy)]
pub enum ObjectErrorKind {
    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "unable to get directory for tempfiles")]
    NoTempDir,

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
    request: FetchFileRequest,
    threadpool: Arc<ThreadPool>,
}

#[derive(Debug, Clone)]
enum FetchFileRequest {
    Sentry(Arc<SentrySourceConfig>, SentryFileId, ObjectId),
    S3(Arc<S3SourceConfig>, DownloadPath, ObjectId),
    Http(Arc<HttpSourceConfig>, DownloadPath, ObjectId),
}

#[derive(Debug, Clone)]
pub struct DownloadPath(String);

#[derive(Debug, Clone)]
pub struct SentryFileId(String);

impl FetchFileRequest {
    fn source(&self) -> SourceConfig {
        match *self {
            FetchFileRequest::Sentry(ref x, ..) => SourceConfig::Sentry(x.clone()),
            FetchFileRequest::S3(ref x, ..) => SourceConfig::S3(x.clone()),
            FetchFileRequest::Http(ref x, ..) => SourceConfig::Http(x.clone()),
        }
    }

    fn object_id(&self) -> &ObjectId {
        match *self {
            FetchFileRequest::Sentry(_, _, ref id) => id,
            FetchFileRequest::S3(_, _, ref id) => id,
            FetchFileRequest::Http(_, _, ref id) => id,
        }
    }
}

impl CacheItemRequest for FetchFile {
    type Item = ObjectFile;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: match self.request {
                FetchFileRequest::Http(ref source, ref path, _) => {
                    format!("{}.{}", source.id, path.0)
                }
                FetchFileRequest::S3(ref source, ref path, _) => {
                    format!("{}.{}", source.id, path.0)
                }
                FetchFileRequest::Sentry(ref source, ref file_id, _) => {
                    format!("{}.{}.sentryinternal", source.id, file_id.0)
                }
            },
            scope: self.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = Scope, Error = Self::Error>> {
        let request = download_from_source(&self.request);
        let path = path.to_owned();
        let request_scope = self.scope.clone();
        let threadpool = self.threadpool.clone();

        let final_scope = if self.request.source().is_public() {
            Scope::Global
        } else {
            request_scope
        };

        let mut cache_key = self.get_cache_key();
        cache_key.scope = final_scope.clone();

        let result = request.and_then(move |payload| {
            if let Some(payload) = payload {
                log::info!("Resolved debug file for {}", cache_key);

                let download_dir = tryf!(path.parent().ok_or(ObjectErrorKind::NoTempDir));
                let download_file = tryf!(tempfile_in(download_dir).context(ObjectErrorKind::Io));

                let future = payload
                    .fold(
                        download_file,
                        clone!(threadpool, |mut file, chunk| threadpool.spawn_handle(
                            future::lazy(move || file.write_all(&chunk).map(|_| file))
                        )),
                    )
                    .and_then(clone!(threadpool, |mut download_file| {
                        threadpool.spawn_handle(future::lazy(move || {
                            // Ensure that both meta data and file contents are available to the
                            // subsequent reads of the file metadata and reads from other threads.
                            download_file.sync_all().context(ObjectErrorKind::Io)?;

                            let metadata = download_file.metadata().context(ObjectErrorKind::Io)?;
                            metric!(time_raw("objects.size") = metadata.len());

                            download_file
                                .seek(SeekFrom::Start(0))
                                .context(ObjectErrorKind::Io)?;
                            let mut magic_bytes: [u8; 4] = [0, 0, 0, 0];
                            download_file
                                .read_exact(&mut magic_bytes)
                                .context(ObjectErrorKind::Io)?;
                            download_file
                                .seek(SeekFrom::Start(0))
                                .context(ObjectErrorKind::Io)?;

                            let mut persist_file =
                                fs::File::create(&path).context(ObjectErrorKind::Io)?;

                            // For a comprehensive list also refer to
                            // https://en.wikipedia.org/wiki/List_of_file_signatures
                            //
                            // XXX: The decoders in the flate2 crate also support being used as a
                            // wrapper around a Write. Only zstd doesn't. If we can get this into
                            // zstd we could save one tempfile and especially avoid the io::copy
                            // for downloads that were not compressed.
                            match magic_bytes {
                                // Magic bytes for zstd
                                // https://tools.ietf.org/id/draft-kucherawy-dispatch-zstd-00.html#rfc.section.2.1.1
                                [0x28, 0xb5, 0x2f, 0xfd] => {
                                    zstd::stream::copy_decode(download_file, persist_file)
                                        .context(ObjectErrorKind::Parsing)?;
                                }
                                // Magic bytes for gzip
                                // https://tools.ietf.org/html/rfc1952#section-2.3.1
                                [0x1f, 0x8b, _, _] => {
                                    // We assume MultiGzDecoder accepts a strict superset of input
                                    // values compared to GzDecoder.
                                    let mut reader =
                                        flate2::read::MultiGzDecoder::new(download_file);
                                    io::copy(&mut reader, &mut persist_file)
                                        .context(ObjectErrorKind::Io)?;
                                }
                                // Magic bytes for zlib
                                [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
                                    let mut reader = flate2::read::ZlibDecoder::new(download_file);
                                    io::copy(&mut reader, &mut persist_file)
                                        .context(ObjectErrorKind::Io)?;
                                }
                                // Probably not compressed
                                _ => {
                                    io::copy(&mut download_file, &mut persist_file)
                                        .context(ObjectErrorKind::Io)?;
                                }
                            }

                            Ok(final_scope)
                        }))
                    }));

                Box::new(future) as Box<dyn Future<Item = _, Error = _>>
            } else {
                log::debug!("No debug file found for {}", cache_key);
                Box::new(Ok(final_scope).into_future()) as Box<dyn Future<Item = _, Error = _>>
            }
        });

        let type_name = self.request.source().type_name();

        Box::new(future_metrics!(
            "objects",
            Some((Duration::from_secs(600), ObjectErrorKind::Timeout.into())),
            result,
            "source_type" => type_name,
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
            let object_id = request.request.object_id();

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
    pub purpose: ObjectPurpose,
    pub scope: Scope,
    pub identifier: ObjectId,
    pub sources: Vec<SourceConfig>,
}

#[derive(Debug, Copy, Clone)]
pub enum ObjectPurpose {
    Unwind,
    Debug,
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
            purpose,
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
                            // Prefer object files with debug/unwind info over object files without
                            // Prefer files that contain an object over unparseable files
                            match response.as_ref().ok().and_then(|o| {
                                let object = o.parse().ok()??;
                                match purpose {
                                    ObjectPurpose::Unwind => Some(object.has_unwind_info()),
                                    ObjectPurpose::Debug => Some(object.has_debug_info()),
                                }
                            }) {
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
    request: &FetchFileRequest,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    match *request {
        FetchFileRequest::Sentry(ref source, ref file_id, _) => {
            sentry::download_from_source(source, file_id)
        }
        FetchFileRequest::Http(ref source, ref file_id, _) => {
            http::download_from_source(source, file_id)
        }
        FetchFileRequest::S3(ref source, ref file_id, _) => {
            s3::download_from_source(source, file_id)
        }
    }
}
