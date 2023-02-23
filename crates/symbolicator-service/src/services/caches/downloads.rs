use std::sync::Arc;

use futures::future::BoxFuture;

use symbolic::common::ByteView;
use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;

use crate::caching::{
    Cache, CacheEntry, CacheItemRequest, CacheKey, CacheVersions, Cacher, SharedCacheRef,
};
use crate::services::download::DownloadService;
use crate::services::fetch_file;
use crate::types::Scope;

use super::versions::DOWNLOADS_CACHE_VERSIONS;

/// Provides cached access to arbitrary downloaded files, without validating or parsing their contents.
///
/// This should be used for any files which are not being validated / converted upon download.
/// An example might be downloads for individual source files.
///
// XXX: The only "validation" that source files might need is checking that they are valid UTF-8.
// Not sure if its worth validating this at download time, to persist a `Malformed` marker?
#[derive(Debug, Clone)]
pub struct CachedDownloads {
    cache: Arc<Cacher<FetchFileRequest>>,
    download_svc: Arc<DownloadService>,
}

impl CachedDownloads {
    /// Creates a new [`CachedDownloads`].
    pub fn new(
        cache: Cache,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        Self {
            cache: Arc::new(Cacher::new(cache, shared_cache)),
            download_svc,
        }
    }

    /// Retrieves the given [`RemoteFile`] from cache, or fetches it and persists it according
    /// to the provided [`Scope`].
    pub async fn fetch_file(
        &self,
        scope: &Scope,
        file: RemoteFile,
    ) -> CacheEntry<ByteView<'static>> {
        let cache_key = CacheKey::from_scoped_file(scope, &file);
        let request = FetchFileRequest {
            file,
            download_svc: Arc::clone(&self.download_svc),
        };

        self.cache.compute_memoized(request, cache_key).await
    }
}

#[derive(Debug, Clone)]
struct FetchFileRequest {
    file: RemoteFile,
    download_svc: Arc<DownloadService>,
}

impl CacheItemRequest for FetchFileRequest {
    type Item = ByteView<'static>;

    const VERSIONS: CacheVersions = DOWNLOADS_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(fetch_file(
            self.download_svc.clone(),
            self.file.clone(),
            temp_file,
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        Ok(data)
    }
}
