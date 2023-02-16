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

use crate::caching::{
    Cache, CacheEntry, CacheError, CacheItemRequest, CacheVersions, Cacher, SharedCacheRef,
};
use crate::services::objects::{
    FindObject, ObjectHandle, ObjectMetaHandle, ObjectPurpose, ObjectsActor,
};
use crate::types::{CandidateStatus, Scope};
use crate::utils::futures::{m, measure};
use crate::utils::sentry::ConfigureScope;

use super::caches::versions::CFICACHE_VERSIONS;
use super::derived::{derive_from_object_handle, DerivedCache};

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
    pub fn new(cache: Cache, shared_cache: SharedCacheRef, objects: ObjectsActor) -> Self {
        CfiCacheActor {
            cficaches: Arc::new(Cacher::new(cache, shared_cache)),
            objects,
        }
    }
}

#[derive(Clone, Debug)]
struct FetchCfiCacheInternal {
    objects_actor: ObjectsActor,
    meta_handle: Arc<ObjectMetaHandle>,
}

/// Extracts the Call Frame Information (CFI) from an object file.
///
/// The extracted CFI is written to `path` in symbolic's
/// [`CfiCache`] format.
#[tracing::instrument(skip_all)]
async fn compute_cficache(
    objects_actor: &ObjectsActor,
    meta_handle: Arc<ObjectMetaHandle>,
    temp_file: &mut NamedTempFile,
) -> CacheEntry {
    let object = objects_actor.fetch(meta_handle).await?;

    write_cficache(temp_file.as_file_mut(), &object)
}

impl CacheItemRequest for FetchCfiCacheInternal {
    type Item = Option<Arc<SymbolFile>>;

    const VERSIONS: CacheVersions = CFICACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        let future = compute_cficache(&self.objects_actor, self.meta_handle.clone(), temp_file);

        let timeout = Duration::from_secs(1200);
        let future = tokio::time::timeout(timeout, future);
        let future = measure("cficaches", m::timed_result, future);
        Box::pin(async move { future.await.map_err(|_| CacheError::Timeout(timeout))? })
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
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
                identifier: request.identifier,
                sources: request.sources,
                scope: request.scope,
                purpose: ObjectPurpose::Unwind,
            })
            .await;

        derive_from_object_handle(found_object, CandidateStatus::Unwind, |meta_handle| {
            let cache_key = meta_handle.cache_key();
            let request = FetchCfiCacheInternal {
                objects_actor: self.objects.clone(),
                meta_handle,
            };
            self.cficaches.compute_memoized(request, cache_key)
        })
        .await
    }
}

/// Extracts the CFI from an object file, writing it to a CFI file.
///
/// The source file is probably an executable or so, the resulting file is in the format of
/// [`CfiCache`].
#[tracing::instrument(skip_all)]
fn write_cficache(file: &mut File, object_handle: &ObjectHandle) -> CacheEntry {
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
