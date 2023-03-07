//! Service for retrieving Artifacts and SourceMap.

use crate::caching::{Cache, Cacher, SharedCacheRef};
use crate::services::download::DownloadService;
use std::sync::Arc;

use super::caches::SourceFilesCache;
use super::sourcemap_lookup::FetchSourceMapCacheInternal;

#[derive(Debug, Clone)]
pub struct SourceMapService {
    pub(crate) sourcefiles_cache: Arc<SourceFilesCache>,
    pub(crate) sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
    pub(crate) download_svc: Arc<DownloadService>,
}

impl SourceMapService {
    pub fn new(
        sourcefiles_cache: Arc<SourceFilesCache>,
        sourcemap_cache: Cache,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        Self {
            sourcefiles_cache,
            sourcemap_caches: Arc::new(Cacher::new(sourcemap_cache, shared_cache)),
            download_svc,
        }
    }
}
