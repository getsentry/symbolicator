use std::cmp;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::time::Duration;

use ::sentry::{configure_scope, Hub};
use failure::Fail;
use futures::future::{FutureExt, TryFutureExt};
use futures01::{future, Future};
use symbolic::common::ByteView;
use symbolic::debuginfo::{self, Archive, Object};
use tempfile::{tempfile_in, NamedTempFile};
use thiserror::Error;

use crate::actors::common::cache::{CacheItemRequest, CachePath, Cacher};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::logging::LogError;
use crate::services::download::{DownloadError, DownloadService, DownloadStatus};
use crate::sources::{FileType, SourceConfig, SourceFileId};
use crate::types::{ObjectFeatures, ObjectId, Scope};
use crate::utils::futures::ThreadPool;
use crate::utils::objects;
use crate::utils::sentry::{SentryFutureExt, WriteSentryScope};

/// Errors happening while fetching objects.
#[derive(Debug, Error)]
pub enum ObjectError {
    #[error("failed to download")]
    Io(#[from] io::Error),

    #[error("failed to download")]
    Download(#[from] DownloadError),

    #[error("failed persisting metadata")]
    Persisting(#[from] serde_json::Error),

    #[error("unable to get directory for tempfiles")]
    NoTempDir,

    #[error("malformed object file")]
    Malformed,

    #[error("failed to parse object")]
    Parsing(#[source] failure::Compat<debuginfo::ObjectError>),

    #[error("failed to look into cache")]
    Caching(#[source] Arc<ObjectError>),

    #[error("object download took too long")]
    Timeout,
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

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>> {
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file meta for {}", cache_key);

        let path = path.to_owned();
        let result = self
            .data_cache
            .compute_memoized(FetchFileDataRequest(self.clone()))
            .map_err(ObjectError::Caching)
            .and_then(move |data| {
                if data.status == CacheStatus::Positive {
                    if let Ok(object) = Object::parse(&data.data) {
                        let mut f = fs::File::create(path)?;

                        let meta = ObjectFeatures {
                            has_debug_info: object.has_debug_info(),
                            has_unwind_info: object.has_unwind_info(),
                            has_symbols: object.has_symbols(),
                            has_sources: object.has_sources(),
                        };

                        log::trace!("Persisting object meta for {}: {:?}", cache_key, meta);
                        serde_json::to_writer(&mut f, &meta)?;
                    }
                }

                Ok(data.status)
            });

        Box::new(result)
    }

    fn should_load(&self, data: &[u8]) -> bool {
        serde_json::from_slice::<ObjectFeatures>(data).is_ok()
    }

    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        let features = serde_json::from_slice(&data).unwrap_or_default();

        ObjectFileMeta {
            request: self.clone(),
            scope,
            features,
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
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file data for {}", cache_key);

        let path = path.to_owned();
        let object_id = self.0.object_id.clone();

        configure_scope(|scope| {
            scope.set_transaction(Some("download_file"));
            self.0.file_id.write_sentry_scope(scope);
        });

        let download_dir = tryf!(path.parent().ok_or(ObjectError::NoTempDir)).to_owned();
        let download_file = tryf!(NamedTempFile::new_in(&download_dir));
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
                    let decompress_result = decompress_object_file(
                        &cache_key,
                        download_file.path(),
                        download_file.reopen()?,
                        tempfile_in(download_dir)?,
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
                            .find(|object| objects::match_id(&object, &object_id));

                        // If we do not find the desired object in this archive - either
                        // because we can't parse any of the objects within, or because none
                        // of the objects match the identifier we're looking for - we return
                        // early.
                        let object = match object_opt {
                            Some(object) => object,
                            None => return Ok(CacheStatus::Negative),
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
}

/// Handle to local cache file of an object.
#[derive(Debug, Clone)]
pub struct ObjectFile {
    object_id: ObjectId,
    scope: Scope,

    file_id: SourceFileId,
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
                Object::parse(&self.data).map_err(|e| ObjectError::Parsing(e.compat()))?,
            )),
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
            .map_err(ObjectError::Caching)
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
                            log::error!("Failed to download from {}: {}", type_name, LogError(&e));
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
                    .map_err(ObjectError::Caching)
                    // Errors from a file download should not make the entire join_all fail. We
                    // collect a Vec<Result> and surface the original error to the user only when
                    // we have no successful downloads.
                    .then(Ok)
                    // create a new hub for each file download
                    .sentry_hub_new_from_current()
            }))
        });

        file_metas.and_then(move |responses: Vec<Result<Arc<ObjectFileMeta>, _>>| {
            responses
                .into_iter()
                .enumerate()
                .min_by_key(|(i, response)| {
                    // Prefer files that contain an object over unparseable files
                    let object = match response {
                        Ok(object) => object,
                        Err(e) => {
                            log::debug!("Failed to download: {}", LogError(e));
                            return (3, *i);
                        }
                    };

                    // Prefer object files with debug/unwind info over object files without
                    let score = match purpose {
                        ObjectPurpose::Unwind if object.features.has_unwind_info => 0,
                        ObjectPurpose::Debug if object.features.has_debug_info => 0,
                        ObjectPurpose::Debug if object.features.has_symbols => 1,
                        ObjectPurpose::Source if object.features.has_sources => 0,
                        _ => 2,
                    };

                    (score, *i)
                })
                .map(|(_, response)| response)
                .transpose()
        })
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
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to decompress cab file",
                ));
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
