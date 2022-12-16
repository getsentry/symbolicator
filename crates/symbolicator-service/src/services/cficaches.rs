use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use minidump_processor::SymbolFile;
use tempfile::NamedTempFile;

use symbolic::cfi::CfiCache;
use symbolic::common::ByteView;
use symbolicator_sources::{FileType, ObjectId, ObjectType, SourceConfig};

use crate::cache::{Cache, CacheEntry, CacheError, ExpirationTime};
use crate::services::cacher::{CacheItemRequest, CacheKey, CacheVersions, Cacher};
use crate::services::objects::{
    FindObject, ObjectHandle, ObjectMetaHandle, ObjectPurpose, ObjectsActor,
};
use crate::types::{CandidateStatus, Scope};
use crate::utils::futures::{m, measure};
use crate::utils::sentry::ConfigureScope;

use super::derived::{derive_from_object_handle, DerivedCache};
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
fn parse_cfi_cache(bytes: ByteView<'static>) -> CacheEntry<Option<Arc<SymbolFile>>> {
    let cfi_cache = CfiCache::from_bytes(bytes).map_err(CacheError::from_std_error)?;

    if cfi_cache.as_slice().is_empty() {
        return Ok(None);
    }

    let symbol_file =
        SymbolFile::from_bytes(cfi_cache.as_slice()).map_err(CacheError::from_std_error)?;

    Ok(Some(Arc::new(symbol_file)))
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
    mut temp_file: NamedTempFile,
) -> CacheEntry<NamedTempFile> {
    let object = objects_actor.fetch(meta_handle).await?;

    write_cficache(temp_file.as_file_mut(), &object)?;

    Ok(temp_file)
}

impl CacheItemRequest for FetchCfiCacheInternal {
    type Item = Option<Arc<SymbolFile>>;

    const VERSIONS: CacheVersions = CFICACHE_VERSIONS;

    fn get_cache_key(&self) -> CacheKey {
        self.meta_handle.cache_key()
    }

    fn compute(&self, temp_file: NamedTempFile) -> BoxFuture<'static, CacheEntry<NamedTempFile>> {
        let future = compute_cficache(
            self.objects_actor.clone(),
            self.meta_handle.clone(),
            temp_file,
        );

        let num_sources = self.request.sources.len().to_string().into();

        let timeout = Duration::from_secs(1200);
        let future = tokio::time::timeout(timeout, future);
        let future = measure(
            "cficaches",
            m::timed_result,
            Some(("num_sources", num_sources)),
            future,
        );
        Box::pin(async move { future.await.map_err(|_| CacheError::Timeout(timeout))? })
    }

    fn should_load(&self, data: &[u8]) -> bool {
        // NOTE: we do *not* check for the `is_latest` version here.
        // If the cficache is parsable, we want to use even outdated versions.
        CfiCache::from_bytes(ByteView::from_slice(data)).is_ok()
    }

    fn load(&self, data: ByteView<'static>, _expiration: ExpirationTime) -> CacheEntry<Self::Item> {
        parse_cfi_cache(data)
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

pub type FetchedCfiCache = DerivedCache<Option<Arc<SymbolFile>>>;

impl CfiCacheActor {
    /// Fetches the CFI cache file for a given code module.
    ///
    /// The code object can be identified by a combination of the code-id, debug-id and
    /// debug filename (the basename).  To do this it looks in the existing cache with the
    /// given scope and if it does not yet exist in cached form will fetch the required DIFs
    /// and compute the required CFI cache file.
    pub async fn fetch(&self, request: FetchCfiCache) -> FetchedCfiCache {
        let found_object = self
            .objects
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Unwind,
            })
            .await;

        derive_from_object_handle(found_object, CandidateStatus::Unwind, |meta_handle| {
            self.cficaches.compute_memoized(FetchCfiCacheInternal {
                request,
                objects_actor: self.objects.clone(),
                meta_handle,
            })
        })
        .await
    }
}

/// Extracts the CFI from an object file, writing it to a CFI file.
///
/// The source file is probably an executable or so, the resulting file is in the format of
/// [`CfiCache`].
#[tracing::instrument(skip_all)]
fn write_cficache(file: &mut File, object_handle: &ObjectHandle) -> CacheEntry<()> {
    object_handle.configure_scope();

    tracing::debug!("Converting cficache for {}", object_handle.cache_key);

    let cficache = CfiCache::from_object(object_handle.object()).map_err(|e| {
        let dynerr: &dyn std::error::Error = &e; // tracing expects a `&dyn Error`
        tracing::error!(error = dynerr, "Could not process CFI Cache");

        CacheError::Malformed(e.to_string())
    })?;

    let mut writer = BufWriter::new(file);
    cficache.write_to(&mut writer)?;

    let file = writer.into_inner().map_err(io::Error::from)?;
    file.sync_all()?;

    Ok(())
}
