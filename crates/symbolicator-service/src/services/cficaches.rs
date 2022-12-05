use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use minidump_processor::SymbolFile;
use thiserror::Error;

use symbolic::cfi::CfiCache;
use symbolic::common::ByteView;
use symbolicator_sources::{FileType, ObjectId, ObjectType, SourceConfig};

use crate::cache::{
    cache_entry_as_cache_status, cache_entry_from_cache_status, Cache, CacheEntry, CacheError,
    CacheStatus, ExpirationTime,
};
use crate::services::cacher::{CacheItemRequest, CacheKey, CacheVersions, Cacher};
use crate::services::objects::{
    FindObject, FoundObject, ObjectError, ObjectHandle, ObjectMetaHandle, ObjectPurpose,
    ObjectsActor,
};
use crate::types::{AllObjectCandidates, ObjectFeatures, ObjectUseInfo, Scope};
use crate::utils::futures::{m, measure};
use crate::utils::sentry::ConfigureScope;

use super::shared_cache::SharedCacheService;

/// The supported cficache versions.
///
/// # How to version
///
/// The initial "unversioned" version is `0`.
/// Whenever we want to increase the version in order to re-generate stale/broken
/// cficaches, we need to:
///
/// * increase the `current` version.
/// * prepend the `current` version to the `fallbacks`.
/// * it is also possible to skip a version, in case a broken deploy needed to
///   be reverted which left behind broken cficaches.
///
/// In case a symbolic update increased its own internal format version, bump the
/// cficache file version as described above, and update the static assertion.
///
/// # Version History
///
/// - `3`: Proactive bump, as a bug in shared cache could have potentially
///   uploaded `v1` cache files as `v2` erroneously.
///
/// - `2`: Allow underflow in Win-x64 CFI which allows loading registers from outside the stack frame.
///
/// - `1`: Generate higher fidelity CFI for Win-x64 binaries.
const CFICACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 3,
    fallbacks: &[2],
};
static_assert!(symbolic::cfi::CFICACHE_LATEST_VERSION == 2);

#[tracing::instrument(skip_all)]
fn parse_cfi_cache(bytes: ByteView<'static>) -> Result<Option<Arc<SymbolFile>>, CacheError> {
    let cfi_cache = CfiCache::from_bytes(bytes).map_err(|e| {
        tracing::error!(error = %e);
        CacheError::InternalError
    })?;

    if cfi_cache.as_slice().is_empty() {
        return Ok(None);
    }

    let symbol_file = SymbolFile::from_bytes(cfi_cache.as_slice()).map_err(|e| {
        tracing::error!(error = %e);
        CacheError::InternalError
    })?;

    Ok(Some(Arc::new(symbol_file)))
}

/// Errors happening while generating a cficache
#[derive(Debug, Error)]
pub enum CfiCacheError {
    #[error("failed to download")]
    Io(#[from] io::Error),

    #[error("failed to download object")]
    Fetching(#[source] ObjectError),

    #[error("failed to parse cficache")]
    Parsing(#[from] symbolic::cfi::CfiError),

    #[error("failed to parse object")]
    ObjectParsing(#[source] ObjectError),

    #[error("failed parsing symbolfile")]
    SymbolFileParsing(#[from] minidump_processor::SymbolError),

    #[error("cficache building took too long")]
    Timeout,
}

impl From<&CfiCacheError> for CacheError {
    fn from(error: &CfiCacheError) -> Self {
        match error {
            CfiCacheError::Io(e) => {
                tracing::error!(error = %e, "failed to write cfi cache");
                Self::InternalError
            }
            CfiCacheError::Fetching(ref e) => Self::DownloadError(e.to_string()),
            CfiCacheError::Parsing(e) => {
                tracing::error!(error = %e, "failed to parse cfi cache");
                Self::InternalError
            }
            CfiCacheError::ObjectParsing(e) => {
                tracing::error!(error = %e, "failed to parse object");
                Self::InternalError
            }
            CfiCacheError::SymbolFileParsing(e) => {
                tracing::error!(error = %e, "failed to parse symbolfile");
                Self::InternalError
            }
            CfiCacheError::Timeout => Self::Timeout(Duration::default()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CfiCacheActor {
    cficaches: Arc<Cacher<FetchCfiCacheInternal>>,
    objects: ObjectsActor,
}

impl CfiCacheActor {
    pub fn new(
        cache: Cache,
        shared_cache_svc: Arc<SharedCacheService>,
        objects: ObjectsActor,
    ) -> Self {
        CfiCacheActor {
            cficaches: Arc::new(Cacher::new(cache, shared_cache_svc)),
            objects,
        }
    }
}

#[derive(Debug)]
pub struct CfiCacheFile {
    features: ObjectFeatures,
    status: CacheStatus,
    data: ByteView<'static>,
    candidates: AllObjectCandidates,
}

impl CfiCacheFile {
    /// Returns the status of this cache file.
    pub fn status(&self) -> &CacheStatus {
        &self.status
    }

    /// Returns the features of the object file this symcache was constructed from.
    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    /// Returns the cfi contents as a [`ByteView`].
    // FIXME(swatinem): symbolic `CfiCache::from_bytes` should actually take a `&[u8]` instead of
    // an explicit `ByteView`.
    pub fn data(&self) -> ByteView {
        self.data.clone()
    }

    /// Returns all the DIF object candidates.
    pub fn candidates(&self) -> &AllObjectCandidates {
        &self.candidates
    }
}

#[derive(Clone, Debug)]
struct FetchCfiCacheInternal {
    request: FetchCfiCache,
    objects_actor: ObjectsActor,
    meta_handle: Arc<ObjectMetaHandle>,
}

/// Extracts the Call Frame Information (CFI) from an object file.
///
/// The extracted CFI is written to `path` in symbolic's
/// [`CfiCache`] format.
#[tracing::instrument(skip_all)]
async fn compute_cficache(
    objects_actor: ObjectsActor,
    meta_handle: Arc<ObjectMetaHandle>,
    path: PathBuf,
) -> Result<CacheStatus, CfiCacheError> {
    let object = objects_actor
        .fetch(meta_handle)
        .await
        .map_err(CfiCacheError::Fetching)?;

    // The original has a download error so the cfi cache entry should just be negative.
    if matches!(object.status(), &CacheStatus::CacheSpecificError(_)) {
        return Ok(CacheStatus::Negative);
    }
    if object.status() != &CacheStatus::Positive {
        return Ok(object.status().clone());
    }

    let status = if let Err(e) = write_cficache(&path, &object) {
        tracing::warn!("Could not write cficache: {}", e);
        sentry::capture_error(&e);

        CacheStatus::Malformed(e.to_string())
    } else {
        CacheStatus::Positive
    };

    Ok(status)
}

impl CacheItemRequest for FetchCfiCacheInternal {
    type Item = CacheEntry<Option<Arc<SymbolFile>>>;
    type Error = CfiCacheError;

    const VERSIONS: CacheVersions = CFICACHE_VERSIONS;

    fn get_cache_key(&self) -> CacheKey {
        self.meta_handle.cache_key()
    }

    fn compute(&self, path: &Path) -> BoxFuture<'static, Result<CacheStatus, Self::Error>> {
        let future = compute_cficache(
            self.objects_actor.clone(),
            self.meta_handle.clone(),
            path.to_owned(),
        );

        let num_sources = self.request.sources.len().to_string().into();

        let future = tokio::time::timeout(Duration::from_secs(1200), future);
        let future = measure(
            "cficaches",
            m::timed_result,
            Some(("num_sources", num_sources)),
            future,
        );
        Box::pin(async move { future.await.map_err(|_| CfiCacheError::Timeout)? })
    }

    fn should_load(&self, data: &[u8]) -> bool {
        // NOTE: we do *not* check for the `is_latest` version here.
        // If the cficache is parsable, we want to use even outdated versions.
        CfiCache::from_bytes(ByteView::from_slice(data)).is_ok()
    }

    fn load(
        &self,
        status: CacheStatus,
        data: ByteView<'static>,
        _expiration: ExpirationTime,
    ) -> Self::Item {
        let cache_entry = cache_entry_from_cache_status(status, data);

        cache_entry.and_then(parse_cfi_cache)
    }
}
#[derive(Debug, Clone)]
pub struct FetchCfiCacheResponse {
    pub cache: CacheEntry<Option<Arc<SymbolFile>>>,
    pub candidates: AllObjectCandidates,
    pub features: ObjectFeatures,
}

impl Default for FetchCfiCacheResponse {
    fn default() -> Self {
        Self {
            cache: Err(CacheError::InternalError),
            candidates: Default::default(),
            features: Default::default(),
        }
    }
}

/// Information for fetching the symbols for this cficache
#[derive(Debug, Clone)]
pub struct FetchCfiCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Arc<[SourceConfig]>,
    pub scope: Scope,
}

impl CfiCacheActor {
    /// Fetches the CFI cache file for a given code module.
    ///
    /// The code object can be identified by a combination of the code-id, debug-id and
    /// debug filename (the basename).  To do this it looks in the existing cache with the
    /// given scope and if it does not yet exist in cached form will fetch the required DIFs
    /// and compute the required CFI cache file.
    pub async fn fetch(&self, request: FetchCfiCache) -> FetchCfiCacheResponse {
        let Ok(FoundObject { meta, mut candidates }) = self
            .objects
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Unwind,
            }).await else {
                return FetchCfiCacheResponse::default();
            };

        match meta {
            Some(meta_handle) => {
                match self
                    .cficaches
                    .compute_memoized(FetchCfiCacheInternal {
                        request,
                        objects_actor: self.objects.clone(),
                        meta_handle: Arc::clone(&meta_handle),
                    })
                    .await
                {
                    Ok(symbolfile) => {
                        candidates.set_unwind(
                            meta_handle.source_id().clone(),
                            &meta_handle.uri(),
                            ObjectUseInfo::from_derived_status(
                                &cache_entry_as_cache_status(&symbolfile),
                                meta_handle.status(),
                            ),
                        );

                        FetchCfiCacheResponse {
                            cache: Arc::try_unwrap(symbolfile).unwrap_or_else(|arc| (*arc).clone()),
                            candidates,
                            features: meta_handle.features(),
                        }
                    }
                    Err(e) => FetchCfiCacheResponse {
                        cache: Err(e.as_ref().into()),
                        candidates,
                        features: ObjectFeatures::default(),
                    },
                }
            }
            None => FetchCfiCacheResponse {
                cache: Err(CacheError::NotFound),
                candidates,
                features: ObjectFeatures::default(),
            },
        }
    }
}

/// Extracts the CFI from an object file, writing it to a CFI file.
///
/// The source file is probably an executable or so, the resulting file is in the format of
/// [`CfiCache`].
#[tracing::instrument(skip_all)]
fn write_cficache(path: &Path, object_handle: &ObjectHandle) -> Result<(), CfiCacheError> {
    object_handle.configure_scope();

    let object = object_handle
        .parse()
        .map_err(CfiCacheError::ObjectParsing)?
        .unwrap();

    let file = File::create(path)?;
    let writer = BufWriter::new(file);

    tracing::debug!("Converting cficache for {}", object_handle.cache_key());

    CfiCache::from_object(&object)?.write_to(writer)?;

    Ok(())
}
