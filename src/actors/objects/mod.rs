use std::error::Error;
use std::fmt;
use std::io;
use std::sync::Arc;

use ::sentry::Hub;
use backtrace::Backtrace;
use futures::future::{FutureExt, TryFutureExt};
use futures01::{future, Future};
use symbolic::debuginfo;

use crate::actors::common::cache::Cacher;
use crate::cache::{Cache, CacheStatus};
use crate::logging::LogError;
use crate::services::download::{DownloadError, DownloadService};
use crate::sources::{FileType, SourceConfig, SourceFileId, SourceId, SourceLocation};
use crate::types::{AllObjectCandidates, ObjectCandidate, ObjectDownloadInfo, ObjectId, Scope};
use crate::utils::futures::ThreadPool;
use crate::utils::sentry::SentryFutureExt;

use data_cache::FetchFileDataRequest;
use meta_cache::FetchFileMetaRequest;

pub use data_cache::ObjectHandle;
pub use meta_cache::ObjectMetaHandle;

mod data_cache;
mod meta_cache;

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

pub struct ObjectFileBytes(pub Arc<ObjectHandle>);

impl AsRef<[u8]> for ObjectFileBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.data
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
    pub meta: Option<Arc<ObjectMetaHandle>>,
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
        shallow_file: Arc<ObjectMetaHandle>,
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
            move |responses: Vec<Result<Arc<ObjectMetaHandle>, CacheLookupError>>|
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
