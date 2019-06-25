use std::cmp;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use failure::{Fail, ResultExt};

use ::sentry::configure_scope;
use ::sentry::integrations::failure::capture_fail;
use futures::{future, Future, IntoFuture, Stream};
use serde::{Deserialize, Serialize};
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Object};
use tempfile::{tempfile_in, NamedTempFile};
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::{CacheItemRequest, Cacher};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::sentry::{SentryFutureExt, WriteSentryScope};
use crate::types::{
    ArcFail, FileType, FilesystemSourceConfig, GcsSourceConfig, HttpSourceConfig, ObjectId,
    S3SourceConfig, Scope, SentrySourceConfig, SourceConfig,
};
use crate::utils::objects;

mod common;
mod filesystem;
mod gcs;
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

/// This requests metadata of a single file at a specific path/url.
#[derive(Clone)]
struct FetchFileMetaRequest {
    /// The scope that the file should be stored under.
    scope: Scope,
    /// Source-type specific attributes.
    file_id: FileId,
    object_id: ObjectId,

    // XXX: This kind of state is not request data. We should find a different way to get this into
    // `<FetchFileMetaRequest as CacheItemRequest>::compute`, e.g. make the Cacher hold arbitrary
    // state for computing.
    threadpool: Arc<ThreadPool>,
    data_cache: Arc<Cacher<FetchFileDataRequest>>,
}

/// This requests the file content of a single file at a specific path/url.
/// The attributes for this are the same as for `FetchFileMetaRequest`, hence the newtype
#[derive(Clone)]
struct FetchFileDataRequest(FetchFileMetaRequest);

#[derive(Debug, Clone)]
enum FileId {
    Sentry(Arc<SentrySourceConfig>, SentryFileId),
    S3(Arc<S3SourceConfig>, DownloadPath),
    Gcs(Arc<GcsSourceConfig>, DownloadPath),
    Http(Arc<HttpSourceConfig>, DownloadPath),
    Filesystem(Arc<FilesystemSourceConfig>, DownloadPath),
}

#[derive(Debug, Clone)]
struct DownloadPath(String);

#[derive(Debug, Clone)]
struct SentryFileId(String);

impl FileId {
    fn source(&self) -> SourceConfig {
        match *self {
            FileId::Sentry(ref x, ..) => SourceConfig::Sentry(x.clone()),
            FileId::S3(ref x, ..) => SourceConfig::S3(x.clone()),
            FileId::Gcs(ref x, ..) => SourceConfig::Gcs(x.clone()),
            FileId::Http(ref x, ..) => SourceConfig::Http(x.clone()),
            FileId::Filesystem(ref x, ..) => SourceConfig::Filesystem(x.clone()),
        }
    }

    fn cache_key(&self) -> String {
        match self {
            FileId::Http(ref source, ref path) => format!("{}.{}", source.id, path.0),
            FileId::S3(ref source, ref path) => format!("{}.{}", source.id, path.0),
            FileId::Gcs(ref source, ref path) => format!("{}.{}", source.id, path.0),
            FileId::Sentry(ref source, ref file_id) => {
                format!("{}.{}.sentryinternal", source.id, file_id.0)
            }
            FileId::Filesystem(ref source, ref path) => format!("{}.{}", source.id, path.0),
        }
    }
}

impl WriteSentryScope for FileId {
    fn write_sentry_scope(&self, scope: &mut ::sentry::Scope) {
        self.source().write_sentry_scope(scope);
    }
}

impl CacheItemRequest for FetchFileMetaRequest {
    type Item = ObjectFileMeta;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.file_id.cache_key(),
            scope: self.scope.clone(),
        }
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>> {
        let path = path.to_owned();

        let result = self
            .data_cache
            .compute_memoized(FetchFileDataRequest(self.clone()))
            .map_err(|e| ObjectError::from(ArcFail(e).context(ObjectErrorKind::Caching)))
            .and_then(move |data| {
                if data.status == CacheStatus::Positive {
                    if let Ok(object) = Object::parse(&data.data) {
                        let mut f = fs::File::create(path).context(ObjectErrorKind::Io)?;
                        let meta = ObjectFileMetaInner {
                            has_debug_info: object.has_debug_info(),
                            has_unwind_info: object.has_unwind_info(),
                            has_symbols: object.has_symbols(),
                            has_source: object.has_source(),
                        };

                        serde_json::to_writer(&mut f, &meta).context(ObjectErrorKind::Io)?;
                    }
                }

                Ok(data.status)
            });

        Box::new(result)
    }

    fn should_load(&self, data: &[u8]) -> bool {
        serde_json::from_slice::<ObjectFileMetaInner>(data).is_ok()
    }

    fn load(&self, scope: Scope, status: CacheStatus, data: ByteView<'static>) -> Self::Item {
        ObjectFileMeta {
            request: self.clone(),
            scope,
            meta: serde_json::from_slice(&data).unwrap_or_default(),
            status,
        }
    }
}

impl CacheItemRequest for FetchFileDataRequest {
    type Item = ObjectFile;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        self.0.get_cache_key()
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>> {
        let request = download_from_source(&self.0.file_id);
        let path = path.to_owned();
        let threadpool = self.0.threadpool.clone();

        let cache_key = self.get_cache_key();

        let object_id = self.0.object_id.clone();

        configure_scope(|scope| {
            scope.set_transaction(Some("download_file"));
            self.0.file_id.write_sentry_scope(scope);
        });

        let result = request.and_then(move |payload| {
            if let Some(payload) = payload {
                log::debug!("Fetching debug file for {}", cache_key);

                let download_dir =
                    tryf!(path.parent().ok_or(ObjectErrorKind::NoTempDir)).to_owned();

                let (download_file_path, download_file) = match payload {
                    DownloadStream::FutureStream(stream) => {
                        let named_download_file = tryf!(
                            NamedTempFile::new_in(&download_dir).context(ObjectErrorKind::Io)
                        );

                        (
                            named_download_file.path().to_owned(),
                            future::Either::A(stream.fold(
                                tryf!(named_download_file.reopen().context(ObjectErrorKind::Io)),
                                clone!(threadpool, |mut file, chunk| threadpool.spawn_handle(
                                    future::lazy(move || file.write_all(&chunk).map(|_| file))
                                )),
                            )),
                        )
                    }
                    DownloadStream::File(path) => (
                        path.clone(),
                        future::Either::B(
                            fs::File::open(&path)
                                .context(ObjectErrorKind::Io)
                                .map_err(From::from)
                                .into_future(),
                        ),
                    ),
                };

                let future = download_file.and_then(clone!(threadpool, |download_file| {
                    log::trace!("Finished download of {}", cache_key);
                    threadpool.spawn_handle(future::lazy(move || {
                        let mut decompressed = decompress_object_file(
                            &cache_key,
                            &download_file_path,
                            download_file,
                            tempfile_in(download_dir).context(ObjectErrorKind::Io)?,
                        )
                        .context(ObjectErrorKind::Io)?;

                        // Seek back to the start and parse this object to we can deal with it.
                        // Since objects in Sentry (and potentially also other sources) might be
                        // multi-arch files (e.g. FatMach), we parse as Archive and try to
                        // extract the wanted file.
                        decompressed
                            .seek(SeekFrom::Start(0))
                            .context(ObjectErrorKind::Io)?;
                        let view = ByteView::map_file(decompressed)?;
                        let archive = match Archive::parse(&view) {
                            Ok(archive) => archive,
                            Err(_) => {
                                return Ok(CacheStatus::Malformed);
                            }
                        };

                        let mut persist_file =
                            fs::File::create(&path).context(ObjectErrorKind::Io)?;

                        if archive.is_multi() {
                            let object_opt = archive
                                .objects()
                                .filter_map(Result::ok)
                                .find(|object| objects::match_id(&object, &object_id));

                            // If we do not find the desired object in this archive - either
                            // because we can't parse any of the objects within, or because none
                            // of the objects match the identifier we're looking for - we return
                            // early.
                            let object = match object_opt {
                                Some(object) => object,
                                None => return Ok(CacheStatus::Negative),
                            };

                            io::copy(&mut object.data(), &mut persist_file)
                                .context(ObjectErrorKind::Io)?;
                        } else {
                            // Attempt to parse the object to capture errors. The result can be
                            // discarded as the object's data is the entire ByteView.
                            if archive.object_by_index(0).is_err() {
                                return Ok(CacheStatus::Malformed);
                            }

                            io::copy(&mut view.as_ref(), &mut persist_file)
                                .context(ObjectErrorKind::Io)?;
                        }

                        Ok(CacheStatus::Positive)
                    }))
                }));

                Box::new(future) as Box<dyn Future<Item = _, Error = _>>
            } else {
                log::debug!("No debug file found for {}", cache_key);
                Box::new(Ok(CacheStatus::Negative).into_future())
                    as Box<dyn Future<Item = _, Error = _>>
            }
        });

        let result = result
            .map_err(|e| {
                capture_fail(e.cause().unwrap_or(&e));
                e
            })
            .sentry_hub_current();

        let type_name = self.0.file_id.source().type_name();

        Box::new(future_metrics!(
            "objects",
            Some((Duration::from_secs(600), ObjectErrorKind::Timeout.into())),
            result,
            "source_type" => type_name,
        ))
    }

    fn load(&self, scope: Scope, status: CacheStatus, data: ByteView<'static>) -> Self::Item {
        let object = ObjectFile {
            object_id: self.0.object_id.clone(),
            scope,

            file_id: self.0.file_id.clone(),
            cache_key: self.get_cache_key(),

            status,
            data,
        };

        configure_scope(|scope| {
            object.write_sentry_scope(scope);
        });

        object
    }
}

pub struct ObjectFileBytes(pub Arc<ObjectFile>);

impl AsRef<[u8]> for ObjectFileBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.data
    }
}

/// Handle to local metadata file of an object. Having an instance of this type does not mean there
/// is a downloaded object file behind it. We cache metadata separately (ObjectFileMetaInner) because
/// every symcache lookup requires reading this metadata.
#[derive(Clone)]
pub struct ObjectFileMeta {
    request: FetchFileMetaRequest,
    scope: Scope,
    meta: ObjectFileMetaInner,
    status: CacheStatus,
}

impl ObjectFileMeta {
    pub fn cache_key(&self) -> CacheKey {
        self.request.get_cache_key()
    }
}

#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize)]
struct ObjectFileMetaInner {
    has_debug_info: bool,
    has_unwind_info: bool,
    has_symbols: bool,
    #[serde(default)]
    has_source: bool,
}

/// Handle to local cache file of an object.
#[derive(Debug, Clone)]
pub struct ObjectFile {
    object_id: ObjectId,
    scope: Scope,

    file_id: FileId,
    cache_key: CacheKey,

    /// The mmapped object.
    data: ByteView<'static>,
    status: CacheStatus,
}

impl ObjectFile {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn has_object(&self) -> bool {
        self.status == CacheStatus::Positive
    }

    pub fn parse(&self) -> Result<Option<Object<'_>>, ObjectError> {
        match self.status {
            CacheStatus::Positive => Ok(Some(
                Object::parse(&self.data).context(ObjectErrorKind::Parsing)?,
            )),
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed => Err(ObjectErrorKind::Parsing.into()),
        }
    }

    pub fn status(&self) -> CacheStatus {
        self.status
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn cache_key(&self) -> &CacheKey {
        &self.cache_key
    }
}

impl WriteSentryScope for ObjectFile {
    fn write_sentry_scope(&self, scope: &mut ::sentry::Scope) {
        self.object_id.write_sentry_scope(scope);
        self.file_id.write_sentry_scope(scope);

        scope.set_tag("object_file.scope", self.scope());

        scope.set_extra(
            "object_file.first_16_bytes",
            format!("{:x?}", &self.data[..cmp::min(self.data.len(), 16)]).into(),
        );
    }
}

#[derive(Clone)]
pub struct ObjectsActor {
    meta_cache: Arc<Cacher<FetchFileMetaRequest>>,
    data_cache: Arc<Cacher<FetchFileDataRequest>>,
    threadpool: Arc<ThreadPool>,
}

impl ObjectsActor {
    pub fn new(meta_cache: Cache, data_cache: Cache, threadpool: Arc<ThreadPool>) -> Self {
        ObjectsActor {
            meta_cache: Arc::new(Cacher::new(meta_cache)),
            data_cache: Arc::new(Cacher::new(data_cache)),
            threadpool,
        }
    }
}

/// Fetch a Object from external sources or internal cache.
#[derive(Debug, Clone)]
pub struct FindObject {
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
    Source,
}

impl ObjectsActor {
    pub fn fetch(
        &self,
        shallow_file: Arc<ObjectFileMeta>,
    ) -> impl Future<Item = Arc<ObjectFile>, Error = ObjectError> {
        self.data_cache
            .compute_memoized(FetchFileDataRequest(shallow_file.request.clone()))
            .map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into())
    }

    pub fn find(
        &self,
        request: FindObject,
    ) -> impl Future<Item = Option<Arc<ObjectFileMeta>>, Error = ObjectError> {
        let FindObject {
            filetypes,
            scope,
            identifier,
            sources,
            purpose,
        } = request;

        let meta_cache = &self.meta_cache;
        let threadpool = &self.threadpool;
        let data_cache = &self.data_cache;

        let prepare_futures: Vec<_> = sources
            .iter()
            .map(move |source| {
                prepare_downloads(source, filetypes, &identifier)
                    .and_then(clone!(
                        meta_cache,
                        data_cache,
                        threadpool,
                        identifier,
                        scope,
                        |ids| {
                            future::join_all(ids.into_iter().map(move |file_id| {
                                let request = FetchFileMetaRequest {
                                    scope: if file_id.source().is_public() {
                                        Scope::Global
                                    } else {
                                        scope.clone()
                                    },
                                    file_id,
                                    object_id: identifier.clone(),
                                    threadpool: threadpool.clone(),
                                    data_cache: data_cache.clone(),
                                };

                                meta_cache
                                    .compute_memoized(request.clone())
                                    .sentry_hub_new_from_current() // new hub because of join_all
                                    .map_err(|e| {
                                        ObjectError::from(
                                            ArcFail(e).context(ObjectErrorKind::Caching),
                                        )
                                    })
                                    .then(Ok)
                            }))
                        }
                    ))
                    .sentry_hub_new_from_current() // new hub because of join_all
            })
            .collect();

        future::join_all(prepare_futures).and_then(move |responses| {
            responses
                .into_iter()
                .flatten()
                .enumerate()
                .min_by_key(|(i, response)| {
                    // Prefer files that contain an object over unparseable files
                    let object = match response {
                        Ok(object) => object,
                        _ => return (3, *i),
                    };

                    // Prefer object files with debug/unwind info over object files without
                    let score = match purpose {
                        ObjectPurpose::Unwind if object.meta.has_unwind_info => 0,
                        ObjectPurpose::Debug if object.meta.has_debug_info => 0,
                        ObjectPurpose::Debug if object.meta.has_symbols => 1,
                        ObjectPurpose::Source if object.meta.has_source => 0,
                        _ => 2,
                    };

                    (score, *i)
                })
                .and_then(|(score, response)| if score < 2 { Some(response) } else { None })
                .transpose()
        })
    }
}

enum DownloadStream {
    FutureStream(Box<dyn Stream<Item = Bytes, Error = ObjectError>>),
    File(PathBuf),
}

fn prepare_downloads(
    source: &SourceConfig,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> Box<Future<Item = Vec<FileId>, Error = ObjectError>> {
    match *source {
        SourceConfig::Sentry(ref source) => sentry::prepare_downloads(source, filetypes, object_id),
        SourceConfig::Http(ref source) => http::prepare_downloads(source, filetypes, object_id),
        SourceConfig::S3(ref source) => s3::prepare_downloads(source, filetypes, object_id),
        SourceConfig::Gcs(ref source) => gcs::prepare_downloads(source, filetypes, object_id),
        SourceConfig::Filesystem(ref source) => {
            filesystem::prepare_downloads(source, filetypes, object_id)
        }
    }
}

fn download_from_source(
    file_id: &FileId,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    match *file_id {
        FileId::Sentry(ref source, ref file_id) => {
            sentry::download_from_source(source.clone(), file_id)
        }
        FileId::Http(ref source, ref file_id) => {
            http::download_from_source(source.clone(), file_id)
        }
        FileId::S3(ref source, ref file_id) => s3::download_from_source(source.clone(), file_id),
        FileId::Gcs(ref source, ref file_id) => gcs::download_from_source(source.clone(), file_id),
        FileId::Filesystem(ref source, ref file_id) => {
            filesystem::download_from_source(source.clone(), file_id)
        }
    }
}

fn decompress_object_file(
    cache_key: &CacheKey,
    download_file_path: &Path,
    mut download_file: fs::File,
    mut extract_file: fs::File,
) -> io::Result<fs::File> {
    // Ensure that both meta data and file contents are available to the
    // subsequent reads of the file metadata and reads from other threads.
    download_file.sync_all()?;

    let metadata = download_file.metadata()?;
    metric!(time_raw("objects.size") = metadata.len());

    download_file.seek(SeekFrom::Start(0))?;
    let mut magic_bytes: [u8; 4] = [0, 0, 0, 0];
    download_file.read_exact(&mut magic_bytes)?;
    download_file.seek(SeekFrom::Start(0))?;

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
            metric!(counter("compression") += 1, "type" => "zstd");
            log::trace!("Decompressing (zstd): {}", cache_key);

            zstd::stream::copy_decode(download_file, &mut extract_file)?;

            Ok(extract_file)
        }
        // Magic bytes for gzip
        // https://tools.ietf.org/html/rfc1952#section-2.3.1
        [0x1f, 0x8b, _, _] => {
            metric!(counter("compression") += 1, "type" => "gz");
            log::trace!("Decompressing (gz): {}", cache_key);

            // We assume MultiGzDecoder accepts a strict superset of input
            // values compared to GzDecoder.
            let mut reader = flate2::read::MultiGzDecoder::new(download_file);
            io::copy(&mut reader, &mut extract_file)?;

            Ok(extract_file)
        }
        // Magic bytes for zlib
        [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
            metric!(counter("compression") += 1, "type" => "zlib");
            log::trace!("Decompressing (zlib): {}", cache_key);

            let mut reader = flate2::read::ZlibDecoder::new(download_file);
            io::copy(&mut reader, &mut extract_file)?;

            Ok(extract_file)
        }
        // Magic bytes for CAB
        [77, 83, 67, 70] => {
            metric!(counter("compression") += 1, "type" => "cab");
            log::trace!("Decompressing (cab): {}", cache_key);

            let status = process::Command::new("cabextract")
                .arg("-sfqp")
                .arg(&download_file_path)
                .stdout(process::Stdio::from(extract_file.try_clone()?))
                .stderr(process::Stdio::null())
                .status()?;

            if !status.success() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to decompress cab file",
                ))?;
            }

            Ok(extract_file)
        }
        // Probably not compressed
        _ => {
            metric!(counter("compression") += 1, "type" => "none");
            log::trace!("No compression detected: {}", cache_key);
            Ok(download_file)
        }
    }
}
