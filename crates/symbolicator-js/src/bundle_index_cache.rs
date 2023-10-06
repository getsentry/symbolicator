use std::sync::Arc;

use symbolic::common::ByteView;
use symbolicator_service::caching::{
    Cache, CacheEntry, CacheItemRequest, CacheKey, CacheVersions, Cacher, SharedCacheRef,
};
use symbolicator_service::services::caches::versions::BUNDLE_INDEX_CACHE_VERSIONS;
use symbolicator_service::services::download::DownloadService;
use symbolicator_service::services::fetch_file;
use symbolicator_service::types::Scope;
use symbolicator_sources::RemoteFile;

use crate::bundle_index::BundleIndex;

type BundleIndexCacheItem = (u32, Arc<BundleIndex>);

#[tracing::instrument(skip_all)]
fn parse_bundle_index(bytes: ByteView<'static>) -> CacheEntry<BundleIndexCacheItem> {
    let index = serde_json::from_slice(&bytes)?;
    let weight = bytes.len().try_into().unwrap_or(u32::MAX);
    Ok((weight, Arc::new(index)))
}

#[derive(Clone, Debug)]
pub struct BundleIndexRequest {
    file: RemoteFile,
    download_svc: Arc<DownloadService>,
}

#[derive(Debug)]
pub struct BundleIndexCache {
    cache: Cacher<BundleIndexRequest>,
    download_svc: Arc<DownloadService>,
}

impl BundleIndexCache {
    pub fn new(
        cache: Cache,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        Self {
            cache: Cacher::new(cache, shared_cache),
            download_svc,
        }
    }

    pub async fn fetch_index(
        &self,
        scope: &Scope,
        file: RemoteFile,
    ) -> CacheEntry<Arc<BundleIndex>> {
        let cache_key = CacheKey::from_scoped_file(scope, &file);

        let request = BundleIndexRequest {
            file,
            download_svc: Arc::clone(&self.download_svc),
        };

        let item = self.cache.compute_memoized(request, cache_key).await?;
        Ok(item.1)
    }
}

impl CacheItemRequest for BundleIndexRequest {
    type Item = BundleIndexCacheItem;

    const VERSIONS: CacheVersions = BUNDLE_INDEX_CACHE_VERSIONS;

    fn compute<'a>(
        &'a self,
        temp_file: &'a mut tempfile::NamedTempFile,
    ) -> futures::future::BoxFuture<'a, CacheEntry> {
        let fut = async {
            fetch_file(self.download_svc.clone(), self.file.clone(), temp_file).await?;

            let view = ByteView::map_file_ref(temp_file.as_file())?;
            parse_bundle_index(view)?;

            Ok(())
        };
        Box::pin(fut)
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        parse_bundle_index(data)
    }

    fn weight(item: &Self::Item) -> u32 {
        item.0.max(std::mem::size_of::<Self::Item>() as u32)
    }

    fn use_shared_cache(&self) -> bool {
        // These change frequently, and are being downloaded from Sentry directly.
        // Here again, the question is whether we want to waste a bit of storage on GCS vs doing
        // more requests to Python.
        true
    }
}
