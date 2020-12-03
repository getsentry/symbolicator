use std::cmp;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::time::Duration;

use ::sentry::{configure_scope, Hub};
use backtrace::Backtrace;
use futures::future::{FutureExt, TryFutureExt};
use futures01::{future, Future};
use symbolic::common::ByteView;
use symbolic::debuginfo::{self, Archive, Object};
use tempfile::{tempfile_in, NamedTempFile};

use crate::actors::common::cache::{CacheItemRequest, CachePath, Cacher};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::logging::LogError;
use crate::services::download::{DownloadError, DownloadService, DownloadStatus};
use crate::sources::{FileType, SourceConfig, SourceFileId, SourceId, SourceLocation};
use crate::types::{
    AllObjectCandidates, ObjectCandidate, ObjectDownloadInfo, ObjectFeatures, ObjectId, Scope,
};
use crate::utils::futures::ThreadPool;
use crate::utils::sentry::{SentryFutureExt, WriteSentryScope};

/// Errors happening while fetching objects.
pub enum ObjectError {
    Io(io::Error, Backtrace),
    Download(DownloadError),
    Persisting(serde_json::Error),
    NoTempDir,
    Malformed,
    Parsing(debuginfo::ObjectError),
    Caching(Arc<ObjectError>),
    Timeout,
}

impl fmt::Display for ObjectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ObjectError: ")?;
        match self {
            ObjectError::Io(_, _) => write!(f, "failed to download (I/O error)")?,
            ObjectError::Download(_) => write!(f, "failed to download")?,
            ObjectError::Persisting(_) => write!(f, "failed persisting data")?,
            ObjectError::NoTempDir => write!(f, "unable to get directory for tempfiles")?,
            ObjectError::Malformed => write!(f, "malformed object file")?,
            ObjectError::Parsing(_) => write!(f, "failed to parse object")?,
            ObjectError::Caching(_) => write!(f, "failed to look into cache")?,
            ObjectError::Timeout => write!(f, "object download took too long")?,
        }
        if f.alternate() {
            if let Some(ref source) = self.source() {
                write!(f, "\n  caused by: ")?;
                fmt::Display::fmt(source, f)?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)?;
        if let ObjectError::Io(_, ref backtrace) = self {
            write!(f, "\n  backtrace:\n{:?}", backtrace)?;
        }
        if f.alternate() {
            if let Some(ref source) = self.source() {
                write!(f, "\n  caused by: ")?;
                fmt::Debug::fmt(source, f)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ObjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ObjectError::Io(ref source, _) => Some(source),
            ObjectError::Download(ref source) => Some(source),
            ObjectError::Persisting(ref source) => Some(source),
            ObjectError::NoTempDir => None,
            ObjectError::Malformed => None,
            ObjectError::Parsing(ref source) => Some(source),
            ObjectError::Caching(ref source) => Some(source.as_ref()),
            ObjectError::Timeout => None,
        }
    }
}

impl From<io::Error> for ObjectError {
    fn from(source: io::Error) -> Self {
        Self::Io(source, Backtrace::new())
    }
}

impl From<DownloadError> for ObjectError {
    fn from(source: DownloadError) -> Self {
        Self::Download(source)
    }
}

impl From<serde_json::Error> for ObjectError {
    fn from(source: serde_json::Error) -> Self {
        Self::Persisting(source)
    }
}

impl From<debuginfo::ObjectError> for ObjectError {
    fn from(source: debuginfo::ObjectError) -> Self {
        Self::Parsing(source)
    }
}

/// Wrapper around [`ObjectError`] to also pass the file information along.
///
/// Because of the requirement of [`CacheItemRequest`] to impl `From<io::Error>` it can not
/// itself contain the [`SourceId`] and [`SourceLocation`].  However we need to carry this
/// along some errors, so we use this wrapper.
#[derive(Debug)]
struct CacheLookupError {
    /// The [`SourceId`] of the object which caused the [`ObjectError`] while being fetched.
    source_id: SourceId,
    /// The [`SourceLocation`] of the object which caused the [`ObjectError`] while being fetched.
    source_location: SourceLocation,
    /// The wrapped [`ObjectError`] which occurred while fetching the object file.
    error: Arc<ObjectError>,
}

/// This requests metadata of a single file at a specific path/url.
#[derive(Clone, Debug)]
struct FetchFileMetaRequest {
    /// The scope that the file should be stored under.
    scope: Scope,
    /// Source-type specific attributes.
    file_id: SourceFileId,
    object_id: ObjectId,

    // XXX: This kind of state is not request data. We should find a different way to get this into
    // `<FetchFileMetaRequest as CacheItemRequest>::compute`, e.g. make the Cacher hold arbitrary
    // state for computing.
    threadpool: ThreadPool,
    data_cache: Arc<Cacher<FetchFileDataRequest>>,
    download_svc: Arc<crate::services::download::DownloadService>,
}

/// This requests the file content of a single file at a specific path/url.
/// The attributes for this are the same as for `FetchFileMetaRequest`, hence the newtype
#[derive(Clone, Debug)]
struct FetchFileDataRequest(FetchFileMetaRequest);

impl CacheItemRequest for FetchFileMetaRequest {
    type Item = ObjectFileMeta;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.file_id.cache_key(),
            scope: self.scope.clone(),
        }
    }

    /// Fetches object file and derives metadata from it, storing this in the cache.
    ///
    /// This uses the data cache to fetch the requested file before parsing it and writing
    /// the object metadata into the cache at `path`.  Technically the data cache could
    /// contain the object file already but this is unlikely as normally the data cache
    /// expires before the metadata cache, so if the metadata needs to be re-computed then
    /// the data cache has probably also expired.
    ///
    /// This returns an error if the download failed.  If the data cache has a
    /// [`CacheStatus::Negative`] or [`CacheStatus::Malformed`] status the same status is
    /// returned.
    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>> {
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file meta for {}", cache_key);

        let path = path.to_owned();
        let result = self
            .data_cache
            .compute_memoized(FetchFileDataRequest(self.clone()))
            .map_err(ObjectError::Caching)
            .and_then(move |object_handle: Arc<ObjectHandle>| {
                if object_handle.status == CacheStatus::Positive {
                    if let Ok(object) = Object::parse(&object_handle.data) {
                        let mut new_cache = fs::File::create(path)?;

                        let meta = ObjectFeatures {
                            has_debug_info: object.has_debug_info(),
                            has_unwind_info: object.has_unwind_info(),
                            has_symbols: object.has_symbols(),
                            has_sources: object.has_sources(),
                        };

                        log::trace!("Persisting object meta for {}: {:?}", cache_key, meta);
                        serde_json::to_writer(&mut new_cache, &meta)?;
                    }
                }

                Ok(object_handle.status)
            });

        Box::new(result)
    }

    fn should_load(&self, data: &[u8]) -> bool {
        serde_json::from_slice::<ObjectFeatures>(data).is_ok()
    }

    /// Returns the [`ObjectFileMeta`] at the given cache key.
    ///
    /// If the `status` is [`CacheStatus::Malformed`] or [`CacheStatus::Negative`] the metadata
    /// returned will contain the default [`ObjectFileMeta::features`].
    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        // When CacheStatus::Negative we get called with an empty ByteView, for Malformed we
        // get the malformed marker.
        let features = match status {
            CacheStatus::Positive => serde_json::from_slice(&data).unwrap_or_else(|err| {
                log::error!(
                    "Failed to load positive ObjectFileMeta cache for {:?}: {:?}",
                    self.get_cache_key(),
                    err
                );
                Default::default()
            }),
            _ => Default::default(),
        };

        ObjectFileMeta {
            request: self.clone(),
            scope,
            features,
            status,
        }
    }
}

impl CacheItemRequest for FetchFileDataRequest {
    type Item = ObjectHandle;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        self.0.get_cache_key()
    }

    /// Downloads the object file, processes it and returns whether the file is in the cache.
    ///
    /// If the object file was successfully downloaded it is first decompressed.  If it is
    /// an archive containing multiple objects, then next the object matching the code or
    /// debug ID of our request is extracted first.  Finally the object is parsed with
    /// symbolic to ensure it is not malformed.
    ///
    /// If there is an error with downloading or decompression then an `Err` of
    /// [`ObjectError`] is returned.  However if only the final object file parsing failed
    /// then an `Ok` with [`CacheStatus::Malformed`] is returned.
    ///
    /// If the object file did not exist on the source a [`CacheStatus::Negative`] will be
    /// returned.
    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>> {
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file data for {}", cache_key);

        let path = path.to_owned();
        let object_id = self.0.object_id.clone();

        configure_scope(|scope| {
            scope.set_transaction(Some("download_file"));
            self.0.file_id.write_sentry_scope(scope);
        });

        let download_file = tryf!(self.0.data_cache.tempfile());
        let download_dir =
            tryf!(download_file.path().parent().ok_or(ObjectError::NoTempDir)).to_owned();
        let request = self
            .0
            .download_svc
            .download(self.0.file_id.clone(), download_file.path().to_owned())
            .compat()
            .map_err(Into::into);

        let result = request.and_then(move |status| -> Result<CacheStatus, ObjectError> {
            match status {
                DownloadStatus::Completed => {
                    log::trace!("Finished download of {}", cache_key);
                    let decompress_result =
                        decompress_object_file(&download_file, tempfile_in(download_dir)?);

                    // Treat decompression errors as malformed files. It is more likely that
                    // the error comes from a corrupt file than a local file system error.
                    let mut decompressed = match decompress_result {
                        Ok(decompressed) => decompressed,
                        Err(_) => return Ok(CacheStatus::Malformed),
                    };

                    // Seek back to the start and parse this object so we can deal with it.
                    // Since objects in Sentry (and potentially also other sources) might be
                    // multi-arch files (e.g. FatMach), we parse as Archive and try to
                    // extract the wanted file.
                    decompressed.seek(SeekFrom::Start(0))?;
                    let view = ByteView::map_file(decompressed)?;
                    let archive = match Archive::parse(&view) {
                        Ok(archive) => archive,
                        Err(_) => {
                            return Ok(CacheStatus::Malformed);
                        }
                    };
                    let mut persist_file = fs::File::create(&path)?;
                    if archive.is_multi() {
                        let object_opt = archive
                            .objects()
                            .filter_map(Result::ok)
                            .find(|object| object_id.match_object(object));

                        let object = match object_opt {
                            Some(object) => object,
                            None => {
                                if archive.objects().any(|r| r.is_err()) {
                                    return Ok(CacheStatus::Malformed);
                                } else {
                                    return Ok(CacheStatus::Negative);
                                }
                            }
                        };

                        io::copy(&mut object.data(), &mut persist_file)?;
                    } else {
                        // Attempt to parse the object to capture errors. The result can be
                        // discarded as the object's data is the entire ByteView.
                        if archive.object_by_index(0).is_err() {
                            return Ok(CacheStatus::Malformed);
                        }

                        io::copy(&mut view.as_ref(), &mut persist_file)?;
                    }

                    Ok(CacheStatus::Positive)
                }
                DownloadStatus::NotFound => {
                    log::debug!("No debug file found for {}", cache_key);
                    Ok(CacheStatus::Negative)
                }
            }
        });

        let result = result
            .map_err(|e| {
                sentry::capture_error(&e);
                e
            })
            .sentry_hub_current();

        let type_name = self.0.file_id.source().type_name();

        Box::new(future_metrics!(
            "objects",
            Some((Duration::from_secs(600), ObjectError::Timeout)),
            result,
            "source_type" => type_name,
        ))
    }

    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        let object = ObjectHandle {
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

pub struct ObjectFileBytes(pub Arc<ObjectHandle>);

impl AsRef<[u8]> for ObjectFileBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.data
    }
}

/// Handle to local metadata file of an object. Having an instance of this type does not mean there
/// is a downloaded object file behind it. We cache metadata separately (ObjectFeatures) because
/// every symcache lookup requires reading this metadata.
#[derive(Clone, Debug)]
pub struct ObjectFileMeta {
    request: FetchFileMetaRequest,
    scope: Scope,
    features: ObjectFeatures,
    status: CacheStatus,
}

impl ObjectFileMeta {
    pub fn cache_key(&self) -> CacheKey {
        self.request.get_cache_key()
    }

    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    pub fn source(&self) -> &SourceId {
        self.request.file_id.source_id()
    }

    pub fn location(&self) -> SourceLocation {
        self.request.file_id.location()
    }

    pub fn status(&self) -> CacheStatus {
        self.status
    }
}

/// Handle to local cache file of an object.
///
/// This handle contains some information identifying the object it is for as well as the
/// cache information.
#[derive(Debug, Clone)]
pub struct ObjectHandle {
    object_id: ObjectId,
    scope: Scope,

    file_id: SourceFileId,
    cache_key: CacheKey,

    /// The mmapped object.
    ///
    /// This only contains the object **if** [`ObjectHandle::status`] is
    /// [`CacheStatus::Positive`], otherwise it will contain an empty string or the special
    /// malformed marker.
    data: ByteView<'static>,
    status: CacheStatus,
}

impl ObjectHandle {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn has_object(&self) -> bool {
        self.status == CacheStatus::Positive
    }

    pub fn parse(&self) -> Result<Option<Object<'_>>, ObjectError> {
        match self.status {
            CacheStatus::Positive => Ok(Some(Object::parse(&self.data)?)),
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed => Err(ObjectError::Malformed),
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

impl WriteSentryScope for ObjectHandle {
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

#[derive(Clone, Debug)]
pub struct ObjectsActor {
    meta_cache: Arc<Cacher<FetchFileMetaRequest>>,
    data_cache: Arc<Cacher<FetchFileDataRequest>>,
    threadpool: ThreadPool,
    download_svc: Arc<DownloadService>,
}

impl ObjectsActor {
    pub fn new(
        meta_cache: Cache,
        data_cache: Cache,
        threadpool: ThreadPool,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        ObjectsActor {
            meta_cache: Arc::new(Cacher::new(meta_cache)),
            data_cache: Arc::new(Cacher::new(data_cache)),
            threadpool,
            download_svc,
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
    pub sources: Arc<[SourceConfig]>,
}

#[derive(Debug, Copy, Clone)]
pub enum ObjectPurpose {
    Unwind,
    Debug,
    Source,
}

/// The response for [`ObjectsActor::find`].
///
/// The found object is in the `meta` field with other DIFs considered in the `candidates`
/// field.
#[derive(Debug, Clone, Default)]
pub struct FoundObject {
    /// If a matching object was found its [`ObjectFileMeta`] will be provided here,
    /// otherwise this will be `None`.
    pub meta: Option<Arc<ObjectFileMeta>>,
    /// This is a list of some meta information on all objects which have been considered
    /// for this object.  It could be populated even if no matching object is found.
    pub candidates: AllObjectCandidates,
}

impl ObjectsActor {
    /// Returns the requested object file.
    ///
    /// This fetches the requested object, re-downloading it from the source if it is no
    /// longer in the cache.
    pub fn fetch(
        &self,
        shallow_file: Arc<ObjectFileMeta>,
    ) -> impl Future<Item = Arc<ObjectHandle>, Error = ObjectError> {
        self.data_cache
            .compute_memoized(FetchFileDataRequest(shallow_file.request.clone()))
            .map_err(ObjectError::Caching)
    }

    /// Fetches matching objects and returns the metadata of the most suitable object.
    ///
    /// This requests the available matching objects from the sources and then looks up the
    /// object metadata of each matching object in the metadata cache.  These are then
    /// ranked and the best matching object metadata is returned.
    ///
    /// Asking for the objects metadata from the data cache also triggers a download of each
    /// object, which will then be cached in the data cache.  The metadata itself is cached
    /// in the metadata cache which usually lives longer.
    pub fn find(
        &self,
        request: FindObject,
    ) -> impl Future<Item = FoundObject, Error = ObjectError> {
        let FindObject {
            filetypes,
            scope,
            identifier,
            sources,
            purpose,
        } = request;

        // Future which creates a vector with all object downloads to try.
        let file_ids = future::join_all(
            sources
                .iter()
                .map(|source| {
                    let type_name = source.type_name();
                    let hub = Arc::new(Hub::new_from_top(Hub::current()));
                    self.download_svc
                        .list_files(source.clone(), filetypes, identifier.clone(), hub)
                        .boxed_local()
                        .compat()
                        .or_else(move |e| {
                            // This basically only happens for the Sentry source type, when doing
                            // the search by debug/code id. We do not surface those errors to the
                            // user (instead we default to an empty search result) and only report
                            // them internally.
                            log::error!(
                                "Failed to fetch file list from {}: {}",
                                type_name,
                                LogError(&e)
                            );
                            Ok(Vec::new())
                        })
                })
                // collect into intermediate vector because of borrowing problems
                .collect::<Vec<_>>(),
        )
        .map(|file_ids: Vec<Vec<SourceFileId>>| -> Vec<SourceFileId> {
            file_ids.into_iter().flatten().collect()
        });

        let meta_cache = self.meta_cache.clone();
        let threadpool = self.threadpool.clone();
        let data_cache = self.data_cache.clone();
        let download_svc = self.download_svc.clone();

        let file_metas = file_ids.and_then(move |file_ids| {
            future::join_all(file_ids.into_iter().map(move |file_id| {
                let source_id = file_id.source_id().to_owned();
                let source_location = file_id.location();
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
                    download_svc: download_svc.clone(),
                };

                meta_cache
                    .compute_memoized(request)
                    .map_err(move |error| CacheLookupError {
                        source_id,
                        source_location,
                        error,
                    })
                    // Errors from a file download should not make the entire join_all fail. We
                    // collect a Vec<Result> and surface the original error to the user only when
                    // we have no successful downloads.
                    .then(Ok)
                    // create a new hub for each file download
                    .sentry_hub_new_from_current()
            }))
        });

        file_metas.and_then(
            move |responses: Vec<Result<Arc<ObjectFileMeta>, CacheLookupError>>|
                                 -> Result<FoundObject, ObjectError> {
                let mut candidates: Vec<ObjectCandidate> = Vec::new();
                responses
                    .into_iter()
                    .map(|response| {
                        match response {
                            Ok(obj_meta) => {
                                let download = match obj_meta.status {
                                    CacheStatus::Positive => ObjectDownloadInfo::Ok {
                                        features: obj_meta.features()
                                    },
                                    CacheStatus::Negative => ObjectDownloadInfo::NotFound,
                                    CacheStatus::Malformed => ObjectDownloadInfo::Malformed,
                                };
                                candidates.push(ObjectCandidate {
                                    source: obj_meta.request.file_id.source_id().clone(),
                                    location: obj_meta.request.file_id.location(),
                                    download,
                                    unwind: Default::default(),
                                    debug: Default::default(),
                                });
                                Ok(obj_meta)
                            }
                            Err(wrapped_error) => {
                                candidates.push(ObjectCandidate {
                                    source: wrapped_error.source_id,
                                    location: wrapped_error.source_location,
                                    download: ObjectDownloadInfo::Error {
                                        details: wrapped_error.error.to_string(),
                                    },
                                    unwind: Default::default(),
                                    debug: Default::default(),
                                });
                                Err(ObjectError::Caching(wrapped_error.error))
                            }
                        }
                    })
                    .filter(|response| match response {
                        // Make sure we only return objects which provide the requested info
                        Ok(object_meta) => {
                            if object_meta.status == CacheStatus::Positive {
                                // object_meta.features is meaningless when CacheStatus != Positive
                                match purpose {
                                    ObjectPurpose::Unwind => object_meta.features.has_unwind_info,
                                    ObjectPurpose::Debug => {
                                        object_meta.features.has_debug_info
                                            || object_meta.features.has_symbols
                                    }
                                    ObjectPurpose::Source => object_meta.features.has_sources,
                                }
                            } else {
                                true
                            }
                        }
                        Err(_) => true,
                    })
                    .enumerate()
                    .min_by_key(|(i, response)| {
                        // The sources are ordered in priority, so for the same quality of
                        // object file prefer a file from an earlier source using the index `i`
                        // provided by enumerate.

                        // Prefer files that contain an object over unparseable files
                        let object_meta = match response {
                            Ok(object_meta) => object_meta,
                            Err(e) => {
                                log::debug!("Failed to download: {:#?}", e);
                                return (3, *i);
                            }
                        };

                        // Prefer object files with debug/unwind info over object files without
                        let score = match purpose {
                            ObjectPurpose::Unwind if object_meta.features.has_unwind_info => 0,
                            ObjectPurpose::Debug if object_meta.features.has_debug_info => 0,
                            ObjectPurpose::Debug if object_meta.features.has_symbols => 1,
                            ObjectPurpose::Source if object_meta.features.has_sources => 0,
                            _ => 2,
                        };
                        (score, *i)
                    })
                    .map(|(_i, response)| response)
                    .transpose()
                    .map(|meta| FoundObject { meta, candidates: candidates.into() })
            },
        )
    }
}

/// Decompresses an object file.
///
/// Some compression methods are implemented by spawning an external tool and can only
/// process from a named pathname, hence we need a [`NamedTempFile`] as source.
fn decompress_object_file(src: &NamedTempFile, mut dst: fs::File) -> io::Result<fs::File> {
    // Ensure that both meta data and file contents are available to the
    // subsequent reads of the file metadata and reads from other threads.
    src.as_file().sync_all()?;

    let metadata = src.as_file().metadata()?;
    metric!(time_raw("objects.size") = metadata.len());

    src.as_file().seek(SeekFrom::Start(0))?;
    let mut magic_bytes: [u8; 4] = [0, 0, 0, 0];
    src.as_file().read_exact(&mut magic_bytes)?;
    src.as_file().seek(SeekFrom::Start(0))?;

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

            zstd::stream::copy_decode(src.as_file(), &mut dst)?;
            Ok(dst)
        }
        // Magic bytes for gzip
        // https://tools.ietf.org/html/rfc1952#section-2.3.1
        [0x1f, 0x8b, _, _] => {
            metric!(counter("compression") += 1, "type" => "gz");

            // We assume MultiGzDecoder accepts a strict superset of input
            // values compared to GzDecoder.
            let mut reader = flate2::read::MultiGzDecoder::new(src.as_file());
            io::copy(&mut reader, &mut dst)?;
            Ok(dst)
        }
        // Magic bytes for zlib
        [0x78, 0x01, _, _] | [0x78, 0x9c, _, _] | [0x78, 0xda, _, _] => {
            metric!(counter("compression") += 1, "type" => "zlib");

            let mut reader = flate2::read::ZlibDecoder::new(src.as_file());
            io::copy(&mut reader, &mut dst)?;
            Ok(dst)
        }
        // Magic bytes for CAB
        [77, 83, 67, 70] => {
            metric!(counter("compression") += 1, "type" => "cab");

            let status = process::Command::new("cabextract")
                .arg("-sfqp")
                .arg(src.path())
                .stdout(process::Stdio::from(dst.try_clone()?))
                .stderr(process::Stdio::null())
                .status()?;

            if !status.success() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to decompress cab file",
                ));
            }

            Ok(dst)
        }
        // Probably not compressed
        _ => {
            metric!(counter("compression") += 1, "type" => "none");
            Ok(src.reopen()?)
        }
    }
}
