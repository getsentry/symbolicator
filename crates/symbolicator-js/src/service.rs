//! Service for retrieving Artifacts and SourceMap.

use std::sync::Arc;

use symbolicator_service::caches::SourceFilesCache;
use symbolicator_service::caching::Cacher;
use symbolicator_service::download::DownloadService;
use symbolicator_service::objects::ObjectsActor;
use symbolicator_service::services::SharedServices;

use crate::api_lookup::SentryLookupApi;
use crate::bundle_index_cache::BundleIndexCache;
use crate::lookup::FetchSourceMapCacheInternal;

#[derive(Debug, Clone)]
pub struct SourceMapService {
    pub(crate) objects: ObjectsActor,
    pub(crate) sourcefiles_cache: Arc<SourceFilesCache>,
    pub(crate) bundle_index_cache: Arc<BundleIndexCache>,
    pub(crate) sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
    pub(crate) download_svc: Arc<DownloadService>,
    pub(crate) api_lookup: Arc<SentryLookupApi>,
}

impl SourceMapService {
    pub fn new(services: &SharedServices) -> Self {
        let caches = &services.caches;
        let shared_cache = services.shared_cache.clone();
        let objects = services.objects.clone();
        let download_svc = services.download_svc.clone();
        let sourcefiles_cache = services.sourcefiles_cache.clone();

        let bundle_index_cache = BundleIndexCache::new(
            caches.bundle_index.clone(),
            shared_cache.clone(),
            download_svc.clone(),
        );

        let in_memory = &services.config.caches.in_memory;
        let api_lookup = Arc::new(SentryLookupApi::new(
            download_svc.trusted_client.clone(),
            download_svc.runtime.clone(),
            download_svc.timeouts,
            in_memory,
        ));

        Self {
            objects,
            sourcefiles_cache,
            bundle_index_cache: Arc::new(bundle_index_cache),
            sourcemap_caches: Arc::new(Cacher::new(caches.sourcemap_caches.clone(), shared_cache)),
            download_svc,
            api_lookup,
        }
    }
}
