use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use thiserror::Error;

use symbolic::common::ByteView;
use symbolic::debuginfo::Object;
use symbolic::ppdb::{PortablePdbCache, PortablePdbCacheConverter};
use symbolicator_sources::{FileType, ObjectId, SourceConfig};

use crate::cache::{Cache, CacheStatus};
use crate::services::objects::ObjectError;
use crate::types::{AllObjectCandidates, ObjectFeatures, ObjectUseInfo, Scope};
use crate::utils::futures::{m, measure};
use crate::utils::sentry::ConfigureScope;

use super::cacher::{CacheItemRequest, CacheKey, CachePath, CacheVersions, Cacher};
use super::objects::{
    FindObject, FoundObject, ObjectHandle, ObjectMetaHandle, ObjectPurpose, ObjectsActor,
};
use super::shared_cache::SharedCacheService;

/// The supported ppdb_cache versions.
///
/// # How to version
///
/// The initial version is `1`.
/// Whenever we want to increase the version in order to re-generate stale/broken
/// ppdb_caches, we need to:
///
/// * increase the `current` version.
/// * prepend the `current` version to the `fallbacks`.
/// * it is also possible to skip a version, in case a broken deploy needed to
///   be reverted which left behind broken ppdb_caches.
///
/// In case a symbolic update increased its own internal format version, bump the
/// ppdb_cache file version as described above, and update the static assertion.
const PPDB_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

/// Errors happening while generating a symcache.
#[derive(Debug, Error)]
pub enum PortablePdbCacheError {
    #[error("failed to write ppdb cache")]
    Io(#[from] io::Error),

    #[error("failed to download object")]
    Fetching(#[source] ObjectError),

    #[error("failed to parse ppdb cache")]
    Parsing(#[source] symbolic::ppdb::CacheError),

    #[error("failed to parse portable pdb")]
    PortablePdbParsing(#[source] ObjectError),

    #[error("failed to write ppdb cache")]
    Writing(#[source] symbolic::ppdb::CacheError),

    #[error("malformed ppdb cache file")]
    Malformed,

    #[error("ppdb cache building took too long")]
    Timeout,
}
#[derive(Debug, Clone)]
pub struct PortablePdbCacheFile {
    data: ByteView<'static>,
    status: CacheStatus,
    candidates: AllObjectCandidates,
    features: ObjectFeatures,
}

impl PortablePdbCacheFile {
    pub fn parse(
        &self,
    ) -> Result<Option<symbolic::ppdb::PortablePdbCache<'_>>, PortablePdbCacheError> {
        match &self.status {
            CacheStatus::Positive => Ok(Some(
                PortablePdbCache::parse(&self.data).map_err(PortablePdbCacheError::Parsing)?,
            )),
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed(_) => Err(PortablePdbCacheError::Malformed),
            // If the cache entry is for a cache specific error, it must be
            // from a previous symcache conversion attempt.
            CacheStatus::CacheSpecificError(_) => Err(PortablePdbCacheError::Malformed),
        }
    }

    /// Returns the list of DIFs which were searched for this ppdb cache.
    pub fn candidates(&self) -> &AllObjectCandidates {
        &self.candidates
    }

    /// Returns the features of the object file this ppdb cache was constructed from.
    pub fn features(&self) -> ObjectFeatures {
        self.features
    }
}

/// Information for fetching the symbols for this ppdb cache
#[derive(Debug, Clone)]
pub struct FetchPortablePdbCache {
    pub identifier: ObjectId,
    pub sources: Arc<[SourceConfig]>,
    pub scope: Scope,
}

#[derive(Clone, Debug)]
pub struct PortablePdbCacheActor {
    ppdb_caches: Arc<Cacher<FetchPortablePdbCacheInternal>>,
    objects: ObjectsActor,
}

impl PortablePdbCacheActor {
    pub fn new(
        cache: Cache,
        shared_cache_svc: Arc<SharedCacheService>,
        objects: ObjectsActor,
    ) -> Self {
        Self {
            ppdb_caches: Arc::new(Cacher::new(cache, shared_cache_svc)),
            objects,
        }
    }

    pub async fn fetch(
        &self,
        request: FetchPortablePdbCache,
    ) -> Result<Arc<PortablePdbCacheFile>, Arc<PortablePdbCacheError>> {
        let FoundObject { meta, candidates } = self
            .objects
            .find(FindObject {
                filetypes: &[FileType::PortablePdb],
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .await
            .map_err(|e| Arc::new(PortablePdbCacheError::Fetching(e)))?;

        match meta {
            Some(handle) => {
                self.ppdb_caches
                    .compute_memoized(FetchPortablePdbCacheInternal {
                        request,
                        objects_actor: self.objects.clone(),
                        object_meta: handle,
                        candidates,
                    })
                    .await
            }
            None => Ok(Arc::new(PortablePdbCacheFile {
                data: ByteView::from_slice(b""),
                status: CacheStatus::Negative,
                candidates,
                features: ObjectFeatures::default(),
            })),
        }
    }
}

#[derive(Clone, Debug)]
struct FetchPortablePdbCacheInternal {
    /// The external request, as passed into [`PortablePdbCacheActor::fetch`].
    request: FetchPortablePdbCache,

    /// The objects actor, used to fetch original DIF objects from.
    objects_actor: ObjectsActor,

    /// ObjectMeta handle of the original DIF object to fetch.
    object_meta: Arc<ObjectMetaHandle>,

    /// The object candidates from which [`FetchPortablePdbCacheInternal::object_meta`] was chosen.
    ///
    /// This needs to be returned back with the symcache result and is only being passed
    /// through here as callers to the PortablePdbCacheActer want to have this info.
    candidates: AllObjectCandidates,
}

/// Fetches the needed DIF objects and spawns symcache computation.
///
/// Required DIF objects are fetched from the objects actor in the current executor, once
/// DIFs have been retrieved it spawns the symcache computation onto the provided
/// threadpool.
///
/// This is the actual implementation of [`CacheItemRequest::compute`] for
/// [`FetchPortablePdbCacheInternal`] but outside of the trait so it can be written as async/await
/// code.
#[tracing::instrument(name = "compute_ppdb_cache", skip_all)]
async fn fetch_difs_and_compute_ppdb_cache(
    path: PathBuf,
    object_meta: Arc<ObjectMetaHandle>,
    objects_actor: ObjectsActor,
) -> Result<CacheStatus, PortablePdbCacheError> {
    let object_handle = objects_actor
        .fetch(object_meta.clone())
        .await
        .map_err(PortablePdbCacheError::Fetching)?;

    // The original has a download error so the sym cache entry should just be negative.
    if matches!(object_handle.status(), &CacheStatus::CacheSpecificError(_)) {
        return Ok(CacheStatus::Negative);
    }

    if object_handle.status() != &CacheStatus::Positive {
        return Ok(object_handle.status().clone());
    }

    let status = match write_ppdb_cache(&path, &object_handle) {
        Ok(_) => CacheStatus::Positive,
        Err(err) => {
            tracing::warn!("Failed to write ppdb_cache: {}", err);
            sentry::capture_error(&err);
            CacheStatus::Malformed(err.to_string())
        }
    };
    Ok(status)
}

impl CacheItemRequest for FetchPortablePdbCacheInternal {
    type Item = PortablePdbCacheFile;
    type Error = PortablePdbCacheError;

    const VERSIONS: CacheVersions = PPDB_CACHE_VERSIONS;

    fn get_cache_key(&self) -> CacheKey {
        self.object_meta.cache_key()
    }

    fn compute(&self, path: &Path) -> BoxFuture<'static, Result<CacheStatus, Self::Error>> {
        let future = fetch_difs_and_compute_ppdb_cache(
            path.to_owned(),
            self.object_meta.clone(),
            self.objects_actor.clone(),
        );

        let num_sources = self.request.sources.len().to_string().into();

        let future = tokio::time::timeout(Duration::from_secs(1200), future);
        let future = measure(
            "ppdb_caches",
            m::timed_result,
            Some(("num_sources", num_sources)),
            future,
        );
        Box::pin(async move { future.await.map_err(|_| PortablePdbCacheError::Timeout)? })
    }

    fn should_load(&self, _data: &[u8]) -> bool {
        true
    }

    fn load(
        &self,
        _scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        let mut candidates = self.candidates.clone(); // yuk!
        candidates.set_debug(
            self.object_meta.source_id(),
            &self.object_meta.uri(),
            ObjectUseInfo::from_derived_status(&status, self.object_meta.status()),
        );

        PortablePdbCacheFile {
            data,
            status,
            candidates,
            features: self.object_meta.features(),
        }
    }
}

/// Computes and writes the ppdb cache.
///
/// It is assumed that the `object_handle` contains a positive cache.
#[tracing::instrument(skip_all)]
fn write_ppdb_cache(
    path: &Path,
    object_handle: &ObjectHandle,
) -> Result<(), PortablePdbCacheError> {
    sentry::configure_scope(|scope| {
        object_handle.to_scope(scope);
    });

    let ppdb_obj = match object_handle.parse() {
        Ok(Some(Object::PortablePdb(ppdb_obj))) => ppdb_obj,
        Ok(Some(_) | None) => panic!("object handle does not contain a valid portable pdb object"),
        Err(e) => return Err(PortablePdbCacheError::PortablePdbParsing(e)),
    };

    tracing::debug!("Converting ppdb cache for {}", object_handle.cache_key());

    let mut converter = PortablePdbCacheConverter::new();

    converter
        .process_portable_pdb(ppdb_obj.portable_pdb())
        .map_err(PortablePdbCacheError::Writing)?;

    let file = File::create(&path)?;
    let mut writer = BufWriter::new(file);
    converter
        .serialize(&mut writer)
        .map_err(PortablePdbCacheError::Io)?;

    let file = writer.into_inner().map_err(io::Error::from)?;

    file.sync_all()?;

    Ok(())
}
