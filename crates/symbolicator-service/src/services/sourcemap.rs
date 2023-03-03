//! Service for retrieving Artifacts and SourceMap.

use symbolicator_sources::SentrySourceConfig;

use crate::caching::{Cache, Cacher, SharedCacheRef};
use crate::services::download::DownloadService;
use crate::types::RawObjectInfo;
use std::sync::Arc;

use super::caches::SourceFilesCache;
use super::sourcemap_lookup::{FetchSourceMapCacheInternal, SourceMapLookup};

#[derive(Debug, Clone)]
pub struct SourceMapService {
    sourcefiles_cache: Arc<SourceFilesCache>,
    sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
    download_svc: Arc<DownloadService>,
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

    pub fn create_sourcemap_lookup(
        &self,
        source: Arc<SentrySourceConfig>,
        modules: &[RawObjectInfo],
        allow_scraping: bool,
    ) -> SourceMapLookup {
        SourceMapLookup::new(
            self.sourcefiles_cache.clone(),
            self.sourcemap_caches.clone(),
            self.download_svc.clone(),
            source,
            modules,
            allow_scraping,
        )
    }
}
