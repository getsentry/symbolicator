use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;

use futures::future::BoxFuture;
use minidump_unwind::SymbolFile;
use sentry::types::DebugId;
use tempfile::NamedTempFile;

use symbolic::cfi::CfiCache;
use symbolic::common::ByteView;
use symbolic::debuginfo::breakpad::BreakpadModuleRecord;
use symbolicator_service::caches::versions::CFICACHE_VERSIONS;
use symbolicator_service::caching::{
    Cache, CacheEntry, CacheError, CacheItemRequest, CacheVersions, Cacher, SharedCacheRef,
};
use symbolicator_service::objects::{
    CandidateStatus, FindObject, ObjectHandle, ObjectMetaHandle, ObjectPurpose, ObjectsActor,
};
use symbolicator_service::types::Scope;
use symbolicator_service::utils::sentry::ConfigureScope;
use symbolicator_sources::{FileType, ObjectId, ObjectType, SourceConfig};

use super::derived::{derive_from_object_handle, DerivedCache};

type CfiItem = Option<Arc<(SymbolFile, Option<CfiModuleInfo>)>>;

pub struct CfiModuleInfo {
    pub debug_id: DebugId,
    pub debug_file: String,
}

#[tracing::instrument(skip_all)]
fn parse_cfi_cache(bytes: ByteView<'static>) -> CacheEntry<(u32, CfiItem)> {
    let weight = bytes.len().try_into().unwrap_or(u32::MAX);
    // NOTE: we estimate the in-memory structures to be ~8x as heavy in memory as on disk
    let weight = weight.saturating_mul(8);

    let cfi_cache = CfiCache::from_bytes(bytes).map_err(CacheError::from_std_error)?;

    if cfi_cache.as_slice().is_empty() {
        return Ok((weight, None));
    }

    let symbol_file =
        SymbolFile::from_bytes(cfi_cache.as_slice()).map_err(CacheError::from_std_error)?;

    let first_line = cfi_cache
        .as_slice()
        .split(|b| *b == b'\n')
        .next()
        .unwrap_or_default();
    let cfi_module = BreakpadModuleRecord::parse(first_line)
        .ok()
        .and_then(|module| {
            let debug_id = module.id.parse().ok()?;
            let debug_file = module.name.into();
            Some(CfiModuleInfo {
                debug_id,
                debug_file,
            })
        });

    Ok((weight, Some(Arc::new((symbol_file, cfi_module)))))
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
    type Item = (u32, CfiItem);

    const VERSIONS: CacheVersions = CFICACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(compute_cficache(
            &self.objects_actor,
            self.meta_handle.clone(),
            temp_file,
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        parse_cfi_cache(data)
    }

    fn weight(item: &Self::Item) -> u32 {
        item.0.max(std::mem::size_of::<Self::Item>() as u32)
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

pub type FetchedCfiCache = DerivedCache<CfiItem>;

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
            async {
                let entry = self
                    .cficaches
                    .compute_memoized(request, cache_key.clone(), cache_key)
                    .await;

                entry.map(|item| item.1)
            }
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
