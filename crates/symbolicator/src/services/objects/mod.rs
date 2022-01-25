use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;
use std::sync::Arc;

use backtrace::Backtrace;
use futures::future;
use sentry::{Hub, SentryFutureExt};
use symbolic::debuginfo;

use crate::cache::{Cache, CacheStatus};
use crate::logging::LogError;
use crate::services::cacher::Cacher;
use crate::services::download::{DownloadError, DownloadService, RemoteDif, RemoteDifUri};
use crate::sources::{FileType, SourceConfig, SourceId};
use crate::types::{AllObjectCandidates, ObjectCandidate, ObjectDownloadInfo, ObjectId, Scope};

use data_cache::FetchFileDataRequest;
use meta_cache::FetchFileMetaRequest;

pub use data_cache::ObjectHandle;
pub use meta_cache::ObjectMetaHandle;

use super::shared_cache::SharedCacheService;

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
/// [`CacheItemRequest`]: crate::services::cacher::CacheItemRequest
/// [`SourceId`]: crate::sources::SourceId
/// [`SourceLocation`]: crate::services::download::SourceLocation
#[derive(Debug)]
struct CacheLookupError {
    /// The object file which was attempted to be fetched.
    file_source: RemoteDif,
    /// The wrapped [`ObjectError`] which occurred while fetching the object file.
    error: Arc<ObjectError>,
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

#[derive(Clone, Debug)]
pub struct ObjectsActor {
    meta_cache: Arc<Cacher<FetchFileMetaRequest>>,
    data_cache: Arc<Cacher<FetchFileDataRequest>>,
    download_svc: Arc<DownloadService>,
}

impl ObjectsActor {
    pub fn new(
        meta_cache: Cache,
        data_cache: Cache,
        shared_cache_svc: Arc<SharedCacheService>,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        ObjectsActor {
            meta_cache: Arc::new(Cacher::new(meta_cache, Arc::clone(&shared_cache_svc))),
            data_cache: Arc::new(Cacher::new(data_cache, shared_cache_svc)),
            download_svc,
        }
    }

    /// Returns the requested object file.
    ///
    /// This fetches the requested object, re-downloading it from the source if it is no
    /// longer in the cache.
    pub async fn fetch(
        &self,
        file_handle: Arc<ObjectMetaHandle>,
    ) -> Result<Arc<ObjectHandle>, ObjectError> {
        let request = FetchFileDataRequest(FetchFileMetaRequest {
            scope: file_handle.scope.clone(),
            file_source: file_handle.file_source.clone(),
            object_id: file_handle.object_id.clone(),
            data_cache: self.data_cache.clone(),
            download_svc: self.download_svc.clone(),
        });

        self.data_cache
            .compute_memoized(request)
            .await
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
    pub async fn find(&self, request: FindObject) -> Result<FoundObject, ObjectError> {
        let FindObject {
            filetypes,
            scope,
            identifier,
            sources,
            purpose,
        } = request;
        let file_ids = self.list_files(&sources, filetypes, &identifier).await;
        let file_metas = self.fetch_file_metas(file_ids, &identifier, scope).await;

        let candidates = create_candidates(&sources, &file_metas);
        let meta = select_meta(file_metas, purpose);

        meta.transpose()
            .map(|meta| FoundObject { meta, candidates })
    }

    /// Collect the list of files to download from all the sources.
    ///
    /// This concurrently contacts all the sources and asks them for the files we should try
    /// to download from them matching the required filetypes and object IDs.  Not all
    /// sources guarantee that the returned files actually exist.
    // TODO(flub): this function should inline already fetch the meta (what
    // `fetch_file_metas()` does now) for each file.  Currently we synchronise all the
    // futures at the end of the listing for no good reason.
    async fn list_files(
        &self,
        sources: &[SourceConfig],
        filetypes: &[FileType],
        identifier: &ObjectId,
    ) -> Vec<RemoteDif> {
        let queries = sources.iter().map(|source| {
            async move {
                let type_name = source.type_name();
                self.download_svc
                    .list_files(source.clone(), filetypes, identifier.clone())
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
            }
            .bind_hub(Hub::new_from_top(Hub::current()))
        });

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
        file_sources: Vec<RemoteDif>,
        identifier: &ObjectId,
        scope: Scope,
    ) -> Vec<Result<Arc<ObjectMetaHandle>, CacheLookupError>> {
        let queries = file_sources.into_iter().map(|file_source| {
            let object_id = identifier.clone();
            let scope = scope.clone();
            let data_cache = self.data_cache.clone();
            let download_svc = self.download_svc.clone();
            let meta_cache = self.meta_cache.clone();

            async move {
                let scope = if file_source.is_public() {
                    Scope::Global
                } else {
                    scope.clone()
                };
                let request = FetchFileMetaRequest {
                    scope,
                    file_source: file_source.clone(),
                    object_id,
                    data_cache,
                    download_svc,
                };
                meta_cache
                    .compute_memoized(request)
                    .await
                    .map_err(|error| CacheLookupError { file_source, error })
            }
            .bind_hub(Hub::new_from_top(Hub::current()))
        });

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
) -> Option<Result<Arc<ObjectMetaHandle>, ObjectError>> {
    let mut selected_meta = None;
    let mut selected_quality = u8::MAX;

    for meta_lookup in all_lookups {
        // Build up the list of candidates, unwrap our error which carried some info just for that.
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

/// Creates collection of all the DIF object candidates used in the metadata lookups.
///
/// If there were any sources which did not return any [`DownloadService::list_files`]
/// results they will get a [`ObjectDownloadInfo::NotFound`] entry with a location of `*`.
/// In practice this will only affect the `sentry` source for now as all other sources
/// always return [`DownloadService::list_files`] results.
fn create_candidates(
    sources: &[SourceConfig],
    lookups: &[Result<Arc<ObjectMetaHandle>, CacheLookupError>],
) -> AllObjectCandidates {
    let mut source_ids: BTreeSet<SourceId> =
        sources.iter().map(|source| source.id()).cloned().collect();
    let mut candidates: Vec<ObjectCandidate> = Vec::with_capacity(lookups.len() + source_ids.len());

    for meta_lookup in lookups.iter() {
        if let Ok(meta_handle) = meta_lookup {
            let source_id = meta_handle.file_source.source_id();
            source_ids.take(source_id);
        }
        candidates.push(create_candidate_info(meta_lookup));
    }

    // Create a NotFound entry for each source from which we did not try and fetch anything.
    for source_id in source_ids {
        let info = ObjectCandidate {
            source: source_id,
            location: RemoteDifUri::new("No object files listed on this source"),
            download: ObjectDownloadInfo::NotFound,
            unwind: Default::default(),
            debug: Default::default(),
        };
        candidates.push(info);
    }

    candidates.into()
}

/// Build the [`ObjectCandidate`] info for the provided meta lookup result.
fn create_candidate_info(
    meta_lookup: &Result<Arc<ObjectMetaHandle>, CacheLookupError>,
) -> ObjectCandidate {
    match meta_lookup {
        Ok(meta_handle) => {
            let download = match &meta_handle.status {
                CacheStatus::Positive => ObjectDownloadInfo::Ok {
                    features: meta_handle.features(),
                },
                CacheStatus::Negative => ObjectDownloadInfo::NotFound,
                CacheStatus::Malformed(_) => ObjectDownloadInfo::Malformed,
                CacheStatus::CacheSpecificError(message) => {
                    match DownloadError::from_cache(&meta_handle.status) {
                        Some(DownloadError::Permissions) => ObjectDownloadInfo::NoPerm {
                            details: String::default(),
                        },
                        Some(_) | None => ObjectDownloadInfo::Error {
                            details: message.clone(),
                        },
                    }
                }
            };
            ObjectCandidate {
                source: meta_handle.file_source.source_id().clone(),
                location: meta_handle.file_source.uri(),
                download,
                unwind: Default::default(),
                debug: Default::default(),
            }
        }
        Err(wrapped_error) => {
            let details = wrapped_error.error.to_string();
            ObjectCandidate {
                source: wrapped_error.file_source.source_id().clone(),
                location: wrapped_error.file_source.uri(),
                download: ObjectDownloadInfo::Error { details },
                unwind: Default::default(),
                debug: Default::default(),
            }
        }
    }
}
