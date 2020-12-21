use std::error::Error;
use std::fmt;
use std::io;
use std::sync::Arc;

use ::sentry::Hub;
use backtrace::Backtrace;
use futures::future::{self, Future, TryFutureExt};
use sentry::SentryFutureExt;
use symbolic::debuginfo;

use crate::actors::common::cache::Cacher;
use crate::cache::{Cache, CacheStatus};
use crate::logging::LogError;
use crate::services::download::{DownloadError, DownloadService};
use crate::sources::{FileType, SourceConfig, SourceFileId, SourceId, SourceLocation};
use crate::types::{AllObjectCandidates, ObjectCandidate, ObjectDownloadInfo, ObjectId, Scope};
use crate::utils::futures::ThreadPool;

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
///
/// [`CacheItemRequest`]: crate::actors::common::cache::CacheItemRequest
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
    /// If a matching object was found its [`ObjectMetaHandle`] will be provided here,
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
    ) -> impl Future<Output = Result<Arc<ObjectHandle>, ObjectError>> {
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
    ///
    /// TODO(flub): Once all the callers are async/await this should take `&self` again
    /// instead of requiring `self`.
    pub async fn find(self, request: FindObject) -> Result<FoundObject, ObjectError> {
        let FindObject {
            filetypes,
            scope,
            identifier,
            sources,
            purpose,
        } = request;

        let file_ids = self.list_files(&sources, filetypes, &identifier).await;
        let file_metas = self.fetch_file_metas(file_ids, &identifier, scope).await;

        select_meta(file_metas, purpose)
    }

    /// Collect the list of files to download from all the sources.
    ///
    /// This concurrently contacts all the sources and asks them for the files we should try
    /// to download from them matching the required filetypes and object IDs.  Not all
    /// sources guarantee that the returned files actually exist.
    // TODO(flub): this function should inline already fetch the meta (what
    // `fetch_file_metas()` does now) for each file.  Currently we synchronise all the
    // futures at the end of the listing for no good reason.
    async fn list_files<'a>(
        &'a self,
        sources: &'a [SourceConfig],
        filetypes: &'static [FileType],
        identifier: &'a ObjectId,
    ) -> Vec<SourceFileId> {
        let mut queries = Vec::with_capacity(sources.len());

        for source in sources.iter() {
            let hub = Arc::new(Hub::new_from_top(Hub::current()));

            let query = async move {
                let type_name = source.type_name();
                self.download_svc
                    .list_files(source.clone(), filetypes, identifier.clone(), hub)
                    .await
                    .unwrap_or_else(|err| {
                        // This basically only happens for the Sentry source type, when doing
                        // the search by debug/code id. We do not surface those errors to the
                        // user (instead we default to an empty search result) and only report
                        // them internally.
                        log::error!(
                            "Failed to fetch file list from {}: {}",
                            type_name,
                            LogError(&err)
                        );
                        Vec::new()
                    })
            };

            queries.push(query);
        }

        future::join_all(queries)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// Fetch all [`ObjectMetaHandle`]s for the files.
    ///
    /// This concurrently looks up the file IDs in the meta-cache and returns all results.
    /// A custom [`CacheLookupError`] is returned to allow us to keep track of the source ID
    /// and source location in case of an error.  [`select_meta`] uses this to build the
    /// [`ObjectCandidate`] list.
    async fn fetch_file_metas(
        &self,
        file_ids: Vec<SourceFileId>,
        identifier: &ObjectId,
        scope: Scope,
    ) -> Vec<Result<Arc<ObjectMetaHandle>, CacheLookupError>> {
        let mut queries = Vec::with_capacity(file_ids.len());

        for file_id in file_ids {
            let object_id = identifier.clone();
            let scope = scope.clone();
            let threadpool = self.threadpool.clone();
            let data_cache = self.data_cache.clone();
            let download_svc = self.download_svc.clone();
            let meta_cache = self.meta_cache.clone();

            let query = async move {
                let scope = if file_id.source().is_public() {
                    Scope::Global
                } else {
                    scope.clone()
                };
                let request = FetchFileMetaRequest {
                    scope,
                    file_id: file_id.clone(),
                    object_id,
                    threadpool,
                    data_cache,
                    download_svc,
                };
                meta_cache
                    .compute_memoized(request)
                    .bind_hub(sentry::Hub::new_from_top(sentry::Hub::current()))
                    .await
                    .map_err(|error| CacheLookupError {
                        source_id: file_id.source_id().to_owned(),
                        source_location: file_id.location(),
                        error,
                    })
            };
            queries.push(query);
        }

        future::join_all(queries).await
    }
}

/// Select the best [ObjectMetaHandle`] out of all lookups from the meta-cache.
///
/// The lookups are expected to be in order or preference, so if two files are equally good
/// the first one will be chosen.  If the file list is emtpy, `None` is returned in the
/// result, if there were no suitable files and only lookup errors one of the lookup errors
/// is propagated.  If there were no suitlable files and no errors `None` is also returned
/// in the result.
fn select_meta(
    all_lookups: Vec<Result<Arc<ObjectMetaHandle>, CacheLookupError>>,
    purpose: ObjectPurpose,
) -> Result<FoundObject, ObjectError> {
    type MetaResult = Result<Arc<ObjectMetaHandle>, ObjectError>;
    let mut candidates: Vec<ObjectCandidate> = Vec::new();
    let mut selected_meta: Option<MetaResult> = None;
    let mut selected_quality = u8::MAX;

    for meta_lookup in all_lookups {
        // Build up the list of candidates, unwrap our error which carried some info just for that.
        candidates.push(create_candidate_info(&meta_lookup));
        let meta_lookup =
            meta_lookup.map_err(|wrapped_err| ObjectError::Caching(wrapped_err.error));

        // Skip objects which and not suitable for what we're asked to provide.  Keep errors
        // though, if we don't find any object we need to return an error.
        if let Ok(ref meta_handle) = meta_lookup {
            if !object_has_features(meta_handle, purpose) {
                continue;
            }
        }

        // We iterate in order of preferred sources, so only select a later object if the
        // quality is better.
        let quality = object_quality(&meta_lookup, purpose);
        if quality < selected_quality {
            selected_meta = Some(meta_lookup);
            selected_quality = quality;
        }
    }

    selected_meta
        .transpose()
        .map(|maybe_meta_handle| FoundObject {
            meta: maybe_meta_handle,
            candidates: candidates.into(),
        })
}

/// Returns a sortable quality measure of this object for the given purpose.
///
/// Lower quality number is better.
fn object_quality(
    meta_lookup: &Result<Arc<ObjectMetaHandle>, ObjectError>,
    purpose: ObjectPurpose,
) -> u8 {
    match meta_lookup {
        Ok(object_meta) => match purpose {
            ObjectPurpose::Unwind if object_meta.features.has_unwind_info => 0,
            ObjectPurpose::Debug if object_meta.features.has_debug_info => 0,
            ObjectPurpose::Debug if object_meta.features.has_symbols => 1,
            ObjectPurpose::Source if object_meta.features.has_sources => 0,
            _ => 2,
        },
        Err(_) => 3,
    }
}

/// Whether the object provides the required features for the given purpose.
fn object_has_features(meta_handle: &ObjectMetaHandle, purpose: ObjectPurpose) -> bool {
    if meta_handle.status == CacheStatus::Positive {
        // object_meta.features is meaningless when CacheStatus != Positive
        match purpose {
            ObjectPurpose::Unwind => meta_handle.features.has_unwind_info,
            ObjectPurpose::Debug => {
                meta_handle.features.has_debug_info || meta_handle.features.has_symbols
            }
            ObjectPurpose::Source => meta_handle.features.has_sources,
        }
    } else {
        true
    }
}

/// Build the [`ObjectCandidate`] info for the provided meta lookup result.
fn create_candidate_info(
    meta_lookup: &Result<Arc<ObjectMetaHandle>, CacheLookupError>,
) -> ObjectCandidate {
    match meta_lookup {
        Ok(meta_handle) => {
            let download = match meta_handle.status {
                CacheStatus::Positive => ObjectDownloadInfo::Ok {
                    features: meta_handle.features(),
                },
                CacheStatus::Negative => ObjectDownloadInfo::NotFound,
                CacheStatus::Malformed => ObjectDownloadInfo::Malformed,
            };
            ObjectCandidate {
                source: meta_handle.request.file_id.source_id().clone(),
                location: meta_handle.request.file_id.location(),
                download,
                unwind: Default::default(),
                debug: Default::default(),
            }
        }
        Err(wrapped_error) => ObjectCandidate {
            source: wrapped_error.source_id.clone(),
            location: wrapped_error.source_location.clone(),
            download: ObjectDownloadInfo::Error {
                details: wrapped_error.error.to_string(),
            },
            unwind: Default::default(),
            debug: Default::default(),
        },
    }
}
