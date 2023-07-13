//! Service for retrieving Artifacts and SourceMap.

use crate::caching::{Cache, Cacher, SharedCacheRef};
use crate::services::download::DownloadService;
use std::sync::Arc;

use super::caches::{BundleIndexCache, SourceFilesCache};
use super::objects::ObjectsActor;
use super::sourcemap_lookup::FetchSourceMapCacheInternal;

#[derive(Debug, Clone)]
pub struct SourceMapService {
    pub(crate) objects: ObjectsActor,
    pub(crate) sourcefiles_cache: Arc<SourceFilesCache>,
    pub(crate) bundle_index_cache: Arc<BundleIndexCache>,
    pub(crate) sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
    pub(crate) download_svc: Arc<DownloadService>,
}

impl SourceMapService {
    pub fn new(
        objects: ObjectsActor,
        sourcefiles_cache: Arc<SourceFilesCache>,
        bundle_index_cache: BundleIndexCache,
        sourcemap_cache: Cache,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        Self {
            objects,
            sourcefiles_cache,
            bundle_index_cache: Arc::new(bundle_index_cache),
            sourcemap_caches: Arc::new(Cacher::new(sourcemap_cache, shared_cache)),
            download_svc,
        }
    }
}
