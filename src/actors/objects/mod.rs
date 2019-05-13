use std::cmp;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use failure::{Fail, ResultExt};

use ::sentry::configure_scope;
use ::sentry::integrations::failure::capture_fail;
use futures::{future, Future, IntoFuture, Stream};
use symbolic::common::ByteView;
use symbolic::debuginfo::Object;
use tempfile::tempfile_in;
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::{CacheItemRequest, Cacher};
use crate::cache::{Cache, CacheKey};
use crate::sentry::{SentryFutureExt, WriteSentryScope};
use crate::types::{
    FileType, HttpSourceConfig, ObjectId, S3SourceConfig, Scope, SentrySourceConfig, SourceConfig,
};

mod common;
mod http;
mod s3;
mod sentry;

const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Fail, Clone, Copy)]
pub enum ObjectErrorKind {
    #[fail(display = "failed to download")]
    Io,

    #[fail(display = "unable to get directory for tempfiles")]
    NoTempDir,

    #[fail(display = "failed dispatch internal message")]
    Mailbox,

    #[fail(display = "failed to parse object")]
    Parsing,

    #[fail(display = "mismatching IDs")]
    IdMismatch,

    #[fail(display = "bad status code")]
    BadStatusCode,

    #[fail(display = "failed to send request to source")]
    SendRequest,

    #[fail(display = "failed to look into cache")]
    Caching,

    #[fail(display = "object download took too long")]
    Timeout,

    #[fail(display = "failed to fetch data from Sentry")]
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

/// The cache item request type yielded by `http::prepare_downloads` and `s3::prepare_downloads`.
/// This requests the download of a single file at a specific path.
#[derive(Debug, Clone)]
pub struct FetchFileRequest {
    /// The scope that the file should be stored under.
    scope: Scope,
    /// Source-type specific attributes.
    request: FetchFileInner,
    object_id: ObjectId,

    // XXX: This kind of state is not request data. We should find a different way to get this into
    // `<FetchFileRequest as CacheItemRequest>::compute`, e.g. make the Cacher hold arbitrary
    // state for computing.
    threadpool: Arc<ThreadPool>,
}

#[derive(Debug, Clone)]
enum FetchFileInner {
    Sentry(Arc<SentrySourceConfig>, SentryFileId),
    S3(Arc<S3SourceConfig>, DownloadPath),
    Http(Arc<HttpSourceConfig>, DownloadPath),
}

#[derive(Debug, Clone)]
pub struct DownloadPath(String);

#[derive(Debug, Clone)]
pub struct SentryFileId(String);

impl FetchFileInner {
    fn source(&self) -> SourceConfig {
        match *self {
            FetchFileInner::Sentry(ref x, ..) => SourceConfig::Sentry(x.clone()),
            FetchFileInner::S3(ref x, ..) => SourceConfig::S3(x.clone()),
            FetchFileInner::Http(ref x, ..) => SourceConfig::Http(x.clone()),
        }
    }
}

impl WriteSentryScope for FetchFileInner {
    fn write_sentry_scope(&self, scope: &mut ::sentry::Scope) {
        self.source().write_sentry_scope(scope);
    }
}

impl CacheItemRequest for FetchFileRequest {
    type Item = ObjectFile;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: match self.request {
                FetchFileInner::Http(ref source, ref path) => format!("{}.{}", source.id, path.0),
                FetchFileInner::S3(ref source, ref path) => format!("{}.{}", source.id, path.0),
                FetchFileInner::Sentry(ref source, ref file_id) => {
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

        configure_scope(|scope| {
            scope.set_transaction(Some("download_file"));
            self.request.write_sentry_scope(scope);
        });

        let result = request.and_then(move |payload| {
            if let Some(payload) = payload {
                log::debug!("Fetching debug file for {}", cache_key);

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
                        log::trace!("Finished download of {}", cache_key);
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
                                    log::trace!("Decompressing (zstd): {}", cache_key);
                                    zstd::stream::copy_decode(download_file, persist_file)
                                        .context(ObjectErrorKind::Parsing)?;
                                }
                                // Magic bytes for gzip
                                // https://tools.ietf.org/html/rfc1952#section-2.3.1
                                [0x1f, 0x8b, _, _] => {
                                    log::trace!("Decompressing (gz): {}", cache_key);

                                    // We assume MultiGzDecoder accepts a strict superset of input
                                    // values compared to GzDecoder.
                                    let mut reader =
                                        flate2::read::MultiGzDecoder::new(download_file);
                                    io::copy(&mut reader, &mut persist_file)
                                        .context(ObjectErrorKind::Io)?;
                                }
                                // Magic bytes for zlib
                                [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
                                    log::trace!("Decompressing (zlib): {}", cache_key);
                                    let mut reader = flate2::read::ZlibDecoder::new(download_file);
                                    io::copy(&mut reader, &mut persist_file)
                                        .context(ObjectErrorKind::Io)?;
                                }
                                // Probably not compressed
                                _ => {
                                    log::trace!("Moving to cache: {}", cache_key);
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

        let result = result
            .map_err(|e| {
                capture_fail(e.cause().unwrap_or(&e));
                e
            })
            .sentry_hub_current();

        let type_name = self.request.source().type_name();

        Box::new(future_metrics!(
            "objects",
            Some((Duration::from_secs(600), ObjectErrorKind::Timeout.into())),
            result,
            "source_type" => type_name,
        ))
    }

    fn load(self, scope: Scope, data: ByteView<'static>) -> Result<Self::Item, Self::Error> {
        let rv = ObjectFile {
            request: Some(self),
            scope,
            object: if data.is_empty() { None } else { Some(data) },
        };

        configure_scope(|scope| {
            rv.write_sentry_scope(scope);
        });

        Ok(rv)
    }
}

/// Handle to local cache file.
#[derive(Debug, Clone)]
pub struct ObjectFile {
    /// The original request. This can be `None` if we
    request: Option<FetchFileRequest>,

    scope: Scope,
    object: Option<ByteView<'static>>,
}

pub struct ObjectFileBytes(pub Arc<ObjectFile>);

impl AsRef<[u8]> for ObjectFileBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.object.as_ref().map_or(&[][..], |x| &x[..])
    }
}

impl ObjectFile {
    pub fn len(&self) -> u64 {
        self.object.as_ref().map_or(0, |x| x.len() as u64)
    }

    pub fn has_object(&self) -> bool {
        self.object.is_some()
    }

    pub fn parse(&self) -> Result<Option<Object<'_>>, ObjectError> {
        let bytes = match self.object {
            Some(ref x) => x,
            None => return Ok(None),
        };
        let parsed = Object::parse(&bytes).context(ObjectErrorKind::Parsing)?;

        if let Some(ref request) = self.request {
            let object_id = &request.object_id;

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
                        "Debug id mismatch. got {}, expected {}",
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
                            "Code id mismatch. got {}, expected {}",
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

    pub fn cache_key(&self) -> Option<CacheKey> {
        self.request.as_ref().map(CacheItemRequest::get_cache_key)
    }
}

impl WriteSentryScope for ObjectFile {
    fn write_sentry_scope(&self, scope: &mut ::sentry::Scope) {
        if let Some(ref request) = self.request {
            request.object_id.write_sentry_scope(scope);
            scope.set_tag("object_file.scope", self.scope());

            request.request.write_sentry_scope(scope);
        }

        if let Some(ref data) = self.object {
            scope.set_extra(
                "object_file.first_16_bytes",
                format!("{:x?}", &data[..cmp::min(data.len(), 16)]).into(),
            );
        }
    }
}

#[derive(Clone)]
pub struct ObjectsActor {
    cache: Arc<Cacher<FetchFileRequest>>,
    threadpool: Arc<ThreadPool>,
}

impl ObjectsActor {
    pub fn new(cache: Cache, threadpool: Arc<ThreadPool>) -> Self {
        ObjectsActor {
            cache: Arc::new(Cacher::new(cache)),
            threadpool,
        }
    }
}

/// Fetch a Object from external sources or internal cache.
#[derive(Debug, Clone)]
pub struct FetchObject {
    pub filetypes: &'static [FileType],
    pub purpose: ObjectPurpose,
    pub scope: Scope,
    pub identifier: ObjectId,
    pub sources: Arc<Vec<SourceConfig>>,
}

#[derive(Debug, Copy, Clone)]
pub enum ObjectPurpose {
    Unwind,
    Debug,
}

impl ObjectsActor {
    pub fn fetch(
        &self,
        request: FetchObject,
    ) -> impl Future<Item = Arc<ObjectFile>, Error = ObjectError> {
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
        })
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
    cache: Arc<Cacher<FetchFileRequest>>,
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
    request: &FetchFileInner,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    match *request {
        FetchFileInner::Sentry(ref source, ref file_id) => {
            sentry::download_from_source(source, file_id)
        }
        FetchFileInner::Http(ref source, ref file_id) => {
            http::download_from_source(source, file_id)
        }
        FetchFileInner::S3(ref source, ref file_id) => s3::download_from_source(source, file_id),
    }
}
