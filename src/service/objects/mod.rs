use std::cmp;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;

use failure::{Fail, ResultExt};

use ::sentry::integrations::failure::capture_fail;
use ::sentry::{configure_scope, Hub};
use futures::{future, Future};
use serde::{Deserialize, Serialize};
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Object};
use tempfile::tempfile_in;

use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::service::cache::{CacheItemRequest, Cacher};
use crate::types::{
    ArcFail, FileType, FilesystemSourceConfig, GcsSourceConfig, HttpSourceConfig, ObjectId,
    S3SourceConfig, Scope, SentrySourceConfig, SourceConfig,
};
use crate::utils::futures::{FutureExt, RemoteThread, SendFuture, TagMap, ThreadPool};
use crate::utils::objects;
use crate::utils::sentry::{SentryFutureExt, ToSentryScope};

mod common;
mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

use self::common::*;
use self::sentry::SentryFileId;

pub use self::common::{ObjectError, ObjectErrorKind};

const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
enum FileId {
    Sentry(Arc<SentrySourceConfig>, SentryFileId),
    S3(Arc<S3SourceConfig>, DownloadPath),
    Gcs(Arc<GcsSourceConfig>, DownloadPath),
    Http(Arc<HttpSourceConfig>, DownloadPath),
    Filesystem(Arc<FilesystemSourceConfig>, DownloadPath),
}

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
            FileId::Http(ref source, ref path) => format!("{}.{}", source.id, path),
            FileId::S3(ref source, ref path) => format!("{}.{}", source.id, path),
            FileId::Gcs(ref source, ref path) => format!("{}.{}", source.id, path),
            FileId::Sentry(ref source, ref file_id) => {
                format!("{}.{}.sentryinternal", source.id, file_id)
            }
            FileId::Filesystem(ref source, ref path) => format!("{}.{}", source.id, path),
        }
    }
}

impl ToSentryScope for FileId {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        self.source().to_scope(scope);
    }
}

/// This requests metadata of a single file at a specific path/url.
#[derive(Clone, Debug)]
struct FetchFileMetaRequest {
    /// The scope that the file should be stored under.
    scope: Scope,
    /// Source-type specific attributes.
    file_id: FileId,
    object_id: ObjectId,

    // XXX: This kind of state is not request data. We should find a different way to get this into
    // `<FetchFileMetaRequest as CacheItemRequest>::compute`, e.g. make the Cacher hold arbitrary
    // state for computing.
    data_cache: Arc<Cacher<FetchFileDataRequest>>,
    downloader: Arc<Downloader>,
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

    fn compute(&self, path: &Path) -> SendFuture<CacheStatus, Self::Error> {
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file meta for {}", cache_key);

        let path = path.to_owned();
        let result = self
            .data_cache
            .compute_memoized(FetchFileDataRequest(self.clone()))
            .map_err(|e| ObjectError::from(ArcFail(e).context(ObjectErrorKind::Caching)))
            .and_then(move |data| {
                if data.status == CacheStatus::Positive {
                    if let Ok(object) = Object::parse(&data.data) {
                        let mut f = File::create(path).context(ObjectErrorKind::Io)?;
                        let meta = ObjectFileMetaInner {
                            has_debug_info: object.has_debug_info(),
                            has_unwind_info: object.has_unwind_info(),
                            has_symbols: object.has_symbols(),
                            has_sources: object.has_sources(),
                        };

                        log::trace!("Persisting object meta for {}: {:?}", cache_key, meta);
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

/// This requests the file content of a single file at a specific path/url.
/// The attributes for this are the same as for `FetchFileMetaRequest`, hence the newtype
#[derive(Clone, Debug)]
struct FetchFileDataRequest(FetchFileMetaRequest);

impl CacheItemRequest for FetchFileDataRequest {
    type Item = ObjectFile;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        self.0.get_cache_key()
    }

    fn compute(&self, path: &Path) -> SendFuture<CacheStatus, Self::Error> {
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file data for {}", cache_key);

        let path = path.to_owned();
        let object_id = self.0.object_id.clone();
        let file_id = self.0.file_id.clone();
        let type_name = file_id.source().type_name();

        configure_scope(|scope| {
            scope.set_transaction(Some("download_file"));
            self.0.file_id.to_scope(scope);
        });

        let downloader = &self.0.downloader;
        let temp_dir = tryf!(path.parent().ok_or(ObjectErrorKind::NoTempDir)).to_owned();

        let future = downloader
            .download(file_id, temp_dir.clone())
            .and_then(move |downloaded_file| {
                handle_object(downloaded_file, &temp_dir, &path, object_id, cache_key)
            })
            .timeout(Duration::from_secs(600), || ObjectErrorKind::Timeout)
            .measure_tagged("objects", TagMap::new().add("source_type", type_name))
            .map_err(|e| {
                capture_fail(e.cause().unwrap_or(&e));
                e
            });

        Box::new(future)
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

        object.configure_scope();
        object
    }
}

/// Handle to local metadata file of an object. Having an instance of this type does not mean there
/// is a downloaded object file behind it. We cache metadata separately (ObjectFileMetaInner) because
/// every symcache lookup requires reading this metadata.
#[derive(Clone, Debug)]
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
    has_sources: bool,
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

    pub fn data(&self) -> ByteView<'static> {
        self.data.clone()
    }
}

impl ToSentryScope for ObjectFile {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        self.object_id.to_scope(scope);
        self.file_id.to_scope(scope);

        scope.set_tag("object_file.scope", self.scope());

        scope.set_extra(
            "object_file.first_16_bytes",
            format!("{:x?}", &self.data[..cmp::min(self.data.len(), 16)]).into(),
        );
    }
}

#[derive(Debug, Copy, Clone)]
pub enum ObjectPurpose {
    Unwind,
    Debug,
    Source,
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

#[derive(Clone, Debug)]
pub struct ObjectsActor {
    meta_cache: Arc<Cacher<FetchFileMetaRequest>>,
    data_cache: Arc<Cacher<FetchFileDataRequest>>,
    downloader: Arc<Downloader>,
}

impl ObjectsActor {
    pub fn new(
        meta_cache: Cache,
        data_cache: Cache,
        cache_pool: ThreadPool,
        downloader: Downloader,
    ) -> Self {
        ObjectsActor {
            meta_cache: Arc::new(Cacher::new(meta_cache, cache_pool.clone())),
            data_cache: Arc::new(Cacher::new(data_cache, cache_pool)),
            downloader: Arc::new(downloader),
        }
    }

    pub fn fetch(
        &self,
        shallow_file: Arc<ObjectFileMeta>,
    ) -> SendFuture<Arc<ObjectFile>, ObjectError> {
        let future = self
            .data_cache
            .compute_memoized(FetchFileDataRequest(shallow_file.request.clone()))
            .map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into());

        Box::new(future)
    }

    pub fn find(
        &self,
        request: FindObject,
    ) -> SendFuture<Option<Arc<ObjectFileMeta>>, ObjectError> {
        let FindObject {
            filetypes,
            scope,
            identifier,
            sources,
            purpose,
        } = request;

        let meta_cache = self.meta_cache.clone();
        let data_cache = self.data_cache.clone();
        let downloader = self.downloader.clone();

        let prepare_futures = sources
            .iter()
            .map(move |source| {
                downloader
                    .list_files(&source, filetypes, &identifier)
                    .and_then(clone!(
                        meta_cache,
                        data_cache,
                        downloader,
                        identifier,
                        scope,
                        |file_ids| {
                            let fetch_futures = file_ids.into_iter().map(move |file_id| {
                                let request = FetchFileMetaRequest {
                                    scope: if file_id.source().is_public() {
                                        Scope::Global
                                    } else {
                                        scope.clone()
                                    },
                                    file_id,
                                    object_id: identifier.clone(),
                                    data_cache: data_cache.clone(),
                                    downloader: downloader.clone(),
                                };

                                meta_cache
                                    .compute_memoized(request.clone())
                                    .map_err(|e| {
                                        ObjectError::from(
                                            ArcFail(e).context(ObjectErrorKind::Caching),
                                        )
                                    })
                                    .then(Ok)
                                    .bind_hub(Hub::new_from_top(Hub::current()))
                            });

                            future::join_all(fetch_futures)
                        }
                    ))
                    .bind_hub(Hub::new_from_top(Hub::current()))
            })
            .collect::<Vec<_>>();

        let selected_future = future::join_all(prepare_futures).and_then(move |responses| {
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
                        ObjectPurpose::Source if object.meta.has_sources => 0,
                        _ => 2,
                    };

                    (score, *i)
                })
                .map(|(_, response)| response)
                .transpose()
        });

        Box::new(selected_future)
    }
}

pub struct Downloader {
    fs: self::filesystem::FilesystemDownloader,
    gcs: self::gcs::GcsDownloader,
    http: self::http::HttpDownloader,
    s3: self::s3::S3Downloader,
    sentry: self::sentry::SentryDownloader,
}

impl Downloader {
    pub fn new() -> Self {
        let thread = RemoteThread::new();

        Self {
            fs: self::filesystem::FilesystemDownloader::new(),
            gcs: self::gcs::GcsDownloader::new(thread.clone()),
            http: self::http::HttpDownloader::new(thread.clone()),
            s3: self::s3::S3Downloader::new(thread.clone()),
            sentry: self::sentry::SentryDownloader::new(thread),
        }
    }

    fn list_files(
        &self,
        source: &SourceConfig,
        filetypes: &'static [FileType],
        object_id: &ObjectId,
    ) -> SendFuture<Vec<FileId>, ObjectError> {
        match *source {
            SourceConfig::Sentry(ref source) => {
                self.sentry.list_files(source.clone(), filetypes, object_id)
            }
            SourceConfig::Http(ref source) => {
                self.http.list_files(source.clone(), filetypes, object_id)
            }
            SourceConfig::S3(ref source) => {
                self.s3.list_files(source.clone(), filetypes, object_id)
            }
            SourceConfig::Gcs(ref source) => {
                self.gcs.list_files(source.clone(), filetypes, object_id)
            }
            SourceConfig::Filesystem(ref source) => {
                self.fs.list_files(source.clone(), filetypes, object_id)
            }
        }
    }

    fn download(
        &self,
        file_id: FileId,
        temp_dir: PathBuf,
    ) -> SendFuture<Option<DownloadedFile>, ObjectError> {
        match file_id {
            FileId::Sentry(source, file_id) => self.sentry.download(source, file_id, temp_dir),
            FileId::Http(source, path) => self.http.download(source, path, temp_dir),
            FileId::S3(source, path) => self.s3.download(source, path, temp_dir),
            FileId::Gcs(source, path) => self.gcs.download(source, path, temp_dir),
            FileId::Filesystem(source, path) => self.fs.download(source, path, temp_dir),
        }
    }
}

impl fmt::Debug for Downloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Downloader")
    }
}

fn decompress_file(
    cache_key: &CacheKey,
    download_file_path: &Path,
    mut download_file: File,
    mut extract_file: File,
) -> io::Result<File> {
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

fn handle_object(
    file_opt: Option<DownloadedFile>,
    temp_path: &Path,
    target_path: &Path,
    object_id: ObjectId,
    cache_key: CacheKey,
) -> Result<CacheStatus, ObjectError> {
    let downloaded_file = match file_opt {
        Some(downloaded_file) => downloaded_file,
        None => {
            log::debug!("No debug file found for {}", cache_key);
            return Ok(CacheStatus::Negative);
        }
    };

    log::trace!("Finished download of {}", cache_key);

    let decompress_result = decompress_file(
        &cache_key,
        downloaded_file.path(),
        downloaded_file.reopen().context(ObjectErrorKind::Io)?,
        tempfile_in(temp_path).context(ObjectErrorKind::Io)?,
    );

    // Treat decompression errors as malformed files. It is more likely that
    // the error comes from a corrupt file than a local file system error.
    let mut decompressed = match decompress_result {
        Ok(decompressed) => decompressed,
        Err(_) => return Ok(CacheStatus::Malformed),
    };

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

    let mut persist_file = File::create(target_path).context(ObjectErrorKind::Io)?;

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

        io::copy(&mut object.data(), &mut persist_file).context(ObjectErrorKind::Io)?;
    } else {
        // Attempt to parse the object to capture errors. The result can be
        // discarded as the object's data is the entire ByteView.
        if archive.object_by_index(0).is_err() {
            return Ok(CacheStatus::Malformed);
        }

        io::copy(&mut view.as_ref(), &mut persist_file).context(ObjectErrorKind::Io)?;
    }

    Ok(CacheStatus::Positive)
}
