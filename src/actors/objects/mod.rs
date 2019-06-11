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
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Object};
use tempfile::{tempfile_in, NamedTempFile};
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::{CacheItemRequest, Cacher};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::sentry::{SentryFutureExt, WriteSentryScope};
use crate::types::{
    FileType, FilesystemSourceConfig, GcsSourceConfig, HttpSourceConfig, ObjectId, S3SourceConfig,
    Scope, SentrySourceConfig, SourceConfig,
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

/// The cache item request type yielded by `http::prepare_downloads` and `s3::prepare_downloads`.
/// This requests the download of a single file at a specific path.
#[derive(Debug, Clone)]
pub struct FetchFileRequest {
    /// The scope that the file should be stored under.
    scope: Scope,
    /// Source-type specific attributes.
    file_id: FileId,
    object_id: ObjectId,

    // XXX: This kind of state is not request data. We should find a different way to get this into
    // `<FetchFileRequest as CacheItemRequest>::compute`, e.g. make the Cacher hold arbitrary
    // state for computing.
    threadpool: Arc<ThreadPool>,
}

#[derive(Debug, Clone)]
enum FileId {
    Sentry(Arc<SentrySourceConfig>, SentryFileId),
    S3(Arc<S3SourceConfig>, DownloadPath),
    Gcs(Arc<GcsSourceConfig>, DownloadPath),
    Http(Arc<HttpSourceConfig>, DownloadPath),
    Filesystem(Arc<FilesystemSourceConfig>, DownloadPath),
}

#[derive(Debug, Clone)]
pub struct DownloadPath(String);

#[derive(Debug, Clone)]
pub struct SentryFileId(String);

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
}

impl WriteSentryScope for FileId {
    fn write_sentry_scope(&self, scope: &mut ::sentry::Scope) {
        self.source().write_sentry_scope(scope);
    }
}

impl CacheItemRequest for FetchFileRequest {
    type Item = ObjectFile;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: match self.file_id {
                FileId::Http(ref source, ref path) => format!("{}.{}", source.id, path.0),
                FileId::S3(ref source, ref path) => format!("{}.{}", source.id, path.0),
                FileId::Gcs(ref source, ref path) => format!("{}.{}", source.id, path.0),
                FileId::Sentry(ref source, ref file_id) => {
                    format!("{}.{}.sentryinternal", source.id, file_id.0)
                }
                FileId::Filesystem(ref source, ref path) => format!("{}.{}", source.id, path.0),
            },
            scope: self.scope.clone(),
        }
    }

    fn compute(
        &self,
        path: &Path,
    ) -> Box<dyn Future<Item = (CacheStatus, Scope), Error = Self::Error>> {
        let request = download_from_source(&self.file_id);
        let path = path.to_owned();
        let request_scope = self.scope.clone();
        let threadpool = self.threadpool.clone();

        let final_scope = if self.file_id.source().is_public() {
            Scope::Global
        } else {
            request_scope
        };

        let mut cache_key = self.get_cache_key();
        cache_key.scope = final_scope.clone();

        let object_id = self.object_id.clone();

        configure_scope(|scope| {
            scope.set_transaction(Some("download_file"));
            self.file_id.write_sentry_scope(scope);
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
                                return Ok((CacheStatus::Malformed, final_scope));
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
                                None => return Ok((CacheStatus::Negative, final_scope)),
                            };

                            io::copy(&mut object.data(), &mut persist_file)
                                .context(ObjectErrorKind::Io)?;
                        } else {
                            // Attempt to parse the object to capture errors. The result can be
                            // discarded as the object's data is the entire ByteView.
                            if archive.object_by_index(0).is_err() {
                                return Ok((CacheStatus::Malformed, final_scope));
                            }

                            io::copy(&mut view.as_ref(), &mut persist_file)
                                .context(ObjectErrorKind::Io)?;
                        }

                        Ok((CacheStatus::Positive, final_scope))
                    }))
                }));

                Box::new(future) as Box<dyn Future<Item = _, Error = _>>
            } else {
                log::debug!("No debug file found for {}", cache_key);
                Box::new(Ok((CacheStatus::Negative, final_scope)).into_future())
                    as Box<dyn Future<Item = _, Error = _>>
            }
        });

        let result = result
            .map_err(|e| {
                capture_fail(e.cause().unwrap_or(&e));
                e
            })
            .sentry_hub_current();

        let type_name = self.file_id.source().type_name();

        Box::new(future_metrics!(
            "objects",
            Some((Duration::from_secs(600), ObjectErrorKind::Timeout.into())),
            result,
            "source_type" => type_name,
        ))
    }

    fn load(&self, scope: Scope, status: CacheStatus, data: ByteView<'static>) -> Self::Item {
        let object = ObjectFile {
            object_id: self.object_id.clone(),
            scope,

            file_id: Some(self.file_id.clone()),
            cache_key: Some(self.get_cache_key()),

            status,
            data: Some(data),
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
        self.0.data.as_ref().map_or(&[][..], |x| &x[..])
    }
}

/// Handle to local cache file.
#[derive(Debug, Clone)]
pub struct ObjectFile {
    object_id: ObjectId,
    scope: Scope,

    file_id: Option<FileId>,
    cache_key: Option<CacheKey>,

    /// The mmapped object.
    data: Option<ByteView<'static>>,
    status: CacheStatus,
}

impl ObjectFile {
    pub fn len(&self) -> u64 {
        self.data.as_ref().map_or(0, |x| x.len() as u64)
    }

    pub fn has_object(&self) -> bool {
        self.status == CacheStatus::Positive
    }

    pub fn parse(&self) -> Result<Option<Object<'_>>, ObjectError> {
        match self.status {
            CacheStatus::Positive => Ok(Some(
                Object::parse(&self.data.as_ref().unwrap()).context(ObjectErrorKind::Parsing)?,
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

    pub fn cache_key(&self) -> Option<&CacheKey> {
        self.cache_key.as_ref()
    }
}

impl WriteSentryScope for ObjectFile {
    fn write_sentry_scope(&self, scope: &mut ::sentry::Scope) {
        self.object_id.write_sentry_scope(scope);

        if let Some(ref file_id) = self.file_id {
            file_id.write_sentry_scope(scope);
        }

        scope.set_tag("object_file.scope", self.scope());

        if let Some(ref data) = self.data {
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
                .sentry_hub_new_from_current() // new hub because of join_all
            })
            .collect();

        future::join_all(prepare_futures).and_then(move |responses| {
            responses
                .into_iter()
                .flatten()
                .enumerate()
                .min_by_key(|(ref i, response)| {
                    // Prefer files that contain an object over unparseable files
                    let object = match response.as_ref().map(|o| o.parse()) {
                        Ok(Ok(Some(object))) => object,
                        _ => return (3, *i),
                    };

                    // Prefer object files with debug/unwind info over object files without
                    let score = match purpose {
                        ObjectPurpose::Unwind if object.has_unwind_info() => 0,
                        ObjectPurpose::Debug if object.has_debug_info() => 0,
                        ObjectPurpose::Debug if object.has_symbols() => 1,
                        _ => 2,
                    };

                    (score, *i)
                })
                .map(|(_, response)| response)
                .unwrap_or_else(clone!(identifier, || {
                    Ok(Arc::new(ObjectFile {
                        scope,
                        object_id: identifier,

                        file_id: None,
                        cache_key: None,

                        data: None,
                        status: CacheStatus::Negative,
                    }))
                }))
        })
    }
}

type PrioritizedDownloads = Vec<Result<Arc<ObjectFile>, ObjectError>>;

pub(crate) enum DownloadStream {
    FutureStream(Box<dyn Stream<Item = Bytes, Error = ObjectError>>),
    File(PathBuf),
}

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
        SourceConfig::Gcs(ref source) => {
            gcs::prepare_downloads(source, scope, filetypes, object_id, threadpool, cache)
        }
        SourceConfig::Filesystem(ref source) => {
            filesystem::prepare_downloads(source, scope, filetypes, object_id, threadpool, cache)
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
