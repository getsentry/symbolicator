use std::sync::Arc;

use futures::future::BoxFuture;

use symbolic::common::{ByteView, SelfCell};
use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;

use crate::caching::{
    Cache, CacheEntry, CacheError, CacheItemRequest, CacheKey, CacheVersions, Cacher,
    SharedCacheRef,
};
use crate::services::download::DownloadService;
use crate::services::fetch_file;
use crate::types::Scope;

use super::versions::SOURCEFILES_CACHE_VERSIONS;

#[derive(Debug, Clone)]
pub struct ByteViewString(SelfCell<ByteView<'static>, &'static str>);

impl std::ops::Deref for ByteViewString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.get()
    }
}

/// Provides cached access to source files, with UTF-8 validating contents.
#[derive(Debug, Clone)]
pub struct SourceFilesCache {
    cache: Arc<Cacher<FetchFileRequest>>,
    download_svc: Arc<DownloadService>,
}

impl SourceFilesCache {
    /// Creates a new [`SourceFilesCache`].
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
    pub async fn fetch_file(&self, scope: &Scope, file: RemoteFile) -> CacheEntry<ByteViewString> {
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
    type Item = ByteViewString;

    const VERSIONS: CacheVersions = SOURCEFILES_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        let fut = async {
            fetch_file(self.download_svc.clone(), self.file.clone(), temp_file).await?;

            let view = ByteView::map_file_ref(temp_file.as_file())?;
            std::str::from_utf8(&view).map_err(|err| CacheError::Malformed(err.to_string()))?;

            Ok(())
        };
        Box::pin(fut)
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        let inner = SelfCell::try_new(data, |s| unsafe {
            std::str::from_utf8(&*s).map_err(|err| CacheError::Malformed(err.to_string()))
        })?;
        Ok(ByteViewString(inner))
    }
}
