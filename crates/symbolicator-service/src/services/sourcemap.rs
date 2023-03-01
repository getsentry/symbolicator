//! Service for retrieving Artifacts and SourceMap.

use symbolicator_sources::SentrySourceConfig;

use crate::caching::{Cache, Cacher, SharedCacheRef};
use crate::services::download::DownloadService;
use std::sync::Arc;

use super::sourcemap_lookup::{
    FetchArtifactCacheInternal, FetchSourceMapCacheInternal, SourceMapLookup,
};

#[derive(Debug, Clone)]
pub struct SourceMapService {
    artifact_caches: Arc<Cacher<FetchArtifactCacheInternal>>,
    sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
    download_svc: Arc<DownloadService>,
}

impl SourceMapService {
    pub fn new(
        artifact_cache: Cache,
        sourcemap_cache: Cache,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        Self {
            artifact_caches: Arc::new(Cacher::new(artifact_cache, shared_cache.clone())),
            sourcemap_caches: Arc::new(Cacher::new(sourcemap_cache, shared_cache)),
            download_svc,
        }
    }

    pub fn create_sourcemap_lookup(&self, source: Arc<SentrySourceConfig>) -> SourceMapLookup {
        SourceMapLookup::new(
            self.artifact_caches.clone(),
            self.sourcemap_caches.clone(),
            self.download_svc.clone(),
            source,
        )
    }
}
