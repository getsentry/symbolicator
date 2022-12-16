use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tempfile::NamedTempFile;
use thiserror::Error;

use symbolic::common::{ByteView, SelfCell};
use symbolic::debuginfo::Object;
use symbolic::ppdb::{PortablePdbCache, PortablePdbCacheConverter};
use symbolicator_sources::{FileType, ObjectId, SourceConfig};

use crate::cache::{Cache, CacheEntry, CacheError, ExpirationTime};
use crate::types::{CandidateStatus, Scope};
use crate::utils::futures::{m, measure};
use crate::utils::sentry::ConfigureScope;

use super::cacher::{CacheItemRequest, CacheKey, CacheVersions, Cacher};
use super::derived::{derive_from_object_handle, DerivedCache};
use super::objects::{FindObject, ObjectHandle, ObjectMetaHandle, ObjectPurpose, ObjectsActor};
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

pub type OwnedPortablePdbCache = SelfCell<ByteView<'static>, PortablePdbCache<'static>>;

fn parse_ppdb_cache_owned(
    byteview: ByteView<'static>,
) -> Result<OwnedPortablePdbCache, CacheError> {
    SelfCell::try_new(byteview, |p| unsafe {
        PortablePdbCache::parse(&*p).map_err(|e| {
            tracing::error!(error = %e);
            CacheError::InternalError
        })
    })
}

/// Errors happening while generating a symcache.
#[derive(Debug, Error)]
pub enum PortablePdbCacheError {
    #[error("failed to write ppdb cache")]
    Io(#[from] io::Error),

    #[error("failed to write ppdb cache")]
    Writing(#[source] symbolic::ppdb::CacheError),
}

impl From<&PortablePdbCacheError> for CacheError {
    fn from(error: &PortablePdbCacheError) -> Self {
        match error {
            PortablePdbCacheError::Io(e) => {
                tracing::error!(error = %e, "failed to write ppdb cache");
                Self::InternalError
            }
            PortablePdbCacheError::Writing(e) => {
                tracing::error!(error = %e, "failed to write ppdb cache");
                Self::InternalError
            }
        }
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
    ) -> DerivedCache<OwnedPortablePdbCache> {
        let found_object = self
            .objects
            .find(FindObject {
                filetypes: &[FileType::PortablePdb],
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .await;
        derive_from_object_handle(found_object, CandidateStatus::Debug, |object_meta| {
            self.ppdb_caches
                .compute_memoized(FetchPortablePdbCacheInternal {
                    request,
                    objects_actor: self.objects.clone(),
                    object_meta,
                })
        })
        .await
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
    mut temp_file: NamedTempFile,
    object_meta: Arc<ObjectMetaHandle>,
    objects_actor: ObjectsActor,
) -> CacheEntry<NamedTempFile> {
    let object_handle = objects_actor.fetch(object_meta.clone()).await?;

    if let Err(err) = write_ppdb_cache(temp_file.as_file_mut(), &object_handle) {
        tracing::warn!("Failed to write ppdb_cache: {}", err);
        sentry::capture_error(&err);

        return Err((&err).into());
    }
    Ok(temp_file)
}

impl CacheItemRequest for FetchPortablePdbCacheInternal {
    type Item = OwnedPortablePdbCache;

    const VERSIONS: CacheVersions = PPDB_CACHE_VERSIONS;

    fn get_cache_key(&self) -> CacheKey {
        self.object_meta.cache_key()
    }

    fn compute(&self, temp_file: NamedTempFile) -> BoxFuture<'static, CacheEntry<NamedTempFile>> {
        let future = fetch_difs_and_compute_ppdb_cache(
            temp_file,
            self.object_meta.clone(),
            self.objects_actor.clone(),
        );

        let num_sources = self.request.sources.len().to_string().into();

        let timeout = Duration::from_secs(1200);
        let future = tokio::time::timeout(timeout, future);
        let future = measure(
            "ppdb_caches",
            m::timed_result,
            Some(("num_sources", num_sources)),
            future,
        );
        Box::pin(async move { future.await.map_err(|_| CacheError::Timeout(timeout))? })
    }

    fn should_load(&self, _data: &[u8]) -> bool {
        true
    }

    fn load(&self, data: ByteView<'static>, _expiration: ExpirationTime) -> CacheEntry<Self::Item> {
        parse_ppdb_cache_owned(data)
    }
}

/// Computes and writes the ppdb cache.
///
/// It is assumed that the `object_handle` contains a positive cache.
#[tracing::instrument(skip_all)]
fn write_ppdb_cache(
    file: &mut File,
    object_handle: &ObjectHandle,
) -> Result<(), PortablePdbCacheError> {
    object_handle.configure_scope();

    let ppdb_obj = match object_handle.object() {
        Object::PortablePdb(ppdb_obj) => ppdb_obj,
        // FIXME(swatinem): instead of panic, we should return an internal error?
        _ => panic!("object handle does not contain a valid portable pdb object"),
    };

    tracing::debug!("Converting ppdb cache for {}", object_handle.cache_key);

    let mut converter = PortablePdbCacheConverter::new();

    converter
        .process_portable_pdb(ppdb_obj.portable_pdb())
        .map_err(PortablePdbCacheError::Writing)?;

    let mut writer = BufWriter::new(file);
    converter
        .serialize(&mut writer)
        .map_err(PortablePdbCacheError::Io)?;

    let file = writer.into_inner().map_err(io::Error::from)?;

    file.sync_all()?;

    Ok(())
}
