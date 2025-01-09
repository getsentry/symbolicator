use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;

use futures::future::BoxFuture;
use tempfile::NamedTempFile;

use symbolic::common::{ByteView, SelfCell};
use symbolic::debuginfo::Object;
use symbolic::ppdb::{PortablePdbCache, PortablePdbCacheConverter};
use symbolicator_service::caches::versions::PPDB_CACHE_VERSIONS;
use symbolicator_service::caching::{
    Cache, CacheEntry, CacheError, CacheItemRequest, CacheVersions, Cacher, SharedCacheRef,
};
use symbolicator_service::objects::{
    CandidateStatus, FindObject, ObjectHandle, ObjectMetaHandle, ObjectPurpose, ObjectsActor,
};
use symbolicator_service::types::Scope;
use symbolicator_service::utils::sentry::ConfigureScope;
use symbolicator_sources::{FileType, ObjectId, SourceConfig};

use super::derived::{derive_from_object_handle, DerivedCache};

pub type OwnedPortablePdbCache = SelfCell<ByteView<'static>, PortablePdbCache<'static>>;

fn parse_ppdb_cache_owned(byteview: ByteView<'static>) -> CacheEntry<OwnedPortablePdbCache> {
    SelfCell::try_new(byteview, |p| unsafe {
        PortablePdbCache::parse(&*p).map_err(CacheError::from_std_error)
    })
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
    pub fn new(cache: Cache, shared_cache: SharedCacheRef, objects: ObjectsActor) -> Self {
        Self {
            ppdb_caches: Arc::new(Cacher::new(cache, shared_cache)),
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
                filetypes: &[FileType::PortablePdb, FileType::Pdb],
                identifier: request.identifier,
                sources: request.sources,
                scope: request.scope,
                purpose: ObjectPurpose::Debug,
            })
            .await;
        derive_from_object_handle(found_object, CandidateStatus::Debug, |object_meta| {
            let cache_key = object_meta.cache_key();
            let request = FetchPortablePdbCacheInternal {
                objects_actor: self.objects.clone(),
                object_meta,
            };
            self.ppdb_caches.compute_memoized(request, cache_key)
        })
        .await
    }
}

#[derive(Clone, Debug)]
struct FetchPortablePdbCacheInternal {
    /// The objects actor, used to fetch original DIF objects from.
    objects_actor: ObjectsActor,

    /// ObjectMeta handle of the original DIF object to fetch.
    object_meta: Arc<ObjectMetaHandle>,
}

#[tracing::instrument(skip_all)]
async fn compute_ppdb_cache(
    temp_file: &mut NamedTempFile,
    objects_actor: &ObjectsActor,
    object_meta: Arc<ObjectMetaHandle>,
) -> CacheEntry {
    let object_handle = objects_actor.fetch(object_meta.clone()).await?;

    write_ppdb_cache(temp_file.as_file_mut(), &object_handle)
}

impl CacheItemRequest for FetchPortablePdbCacheInternal {
    type Item = OwnedPortablePdbCache;

    const VERSIONS: CacheVersions = PPDB_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(compute_ppdb_cache(
            temp_file,
            &self.objects_actor,
            self.object_meta.clone(),
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        parse_ppdb_cache_owned(data)
    }
}

/// Computes and writes the ppdb cache.
///
/// It is assumed that the `object_handle` contains a positive cache.
#[tracing::instrument(skip_all)]
fn write_ppdb_cache(file: &mut File, object_handle: &ObjectHandle) -> CacheEntry {
    object_handle.configure_scope();

    let ppdb_obj = match object_handle.object() {
        Object::PortablePdb(ppdb_obj) => ppdb_obj,
        _ => {
            tracing::warn!("Trying to symbolicate a .NET event with a non-PPDB object file");
            return Err(CacheError::Unsupported(
                "Only portable PDB files can be used for .NET symbolication".to_owned(),
            ));
        }
    };

    tracing::debug!("Converting ppdb cache for {}", object_handle.cache_key);

    let mut converter = PortablePdbCacheConverter::new();
    converter
        .process_portable_pdb(ppdb_obj.portable_pdb())
        .map_err(|e| {
            let dynerr: &dyn std::error::Error = &e; // tracing expects a `&dyn Error`
            tracing::error!(error = dynerr, "Could not process PortablePDB Cache");

            CacheError::Malformed(e.to_string())
        })?;

    let mut writer = BufWriter::new(file);
    converter.serialize(&mut writer)?;
    let file = writer.into_inner().map_err(io::Error::from)?;
    file.sync_all()?;

    // Parse the ppdbcache file to verify integrity
    let bv = ByteView::map_file_ref(file)?;
    if PortablePdbCache::parse(&bv).is_err() {
        tracing::error!("Failed to verify integrity of freshly written PortablePDB Cache");
        return Err(CacheError::InternalError);
    }

    Ok(())
}
