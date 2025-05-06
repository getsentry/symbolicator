use std::sync::Arc;

use futures::future::BoxFuture;

use symbolic::common::{ByteView, SelfCell};
use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;

use crate::caches::CacheVersions;
use crate::caching::{
    Cache, CacheContents, CacheEntry, CacheError, CacheItemRequest, CacheKey, Cacher,
    SharedCacheRef,
};
use crate::download::{DownloadService, fetch_file};
use crate::types::Scope;

use super::versions::SOURCEFILES_CACHE_VERSIONS;

#[derive(Debug, Clone)]
pub struct ByteViewString(SelfCell<ByteView<'static>, &'static str>);

impl From<String> for ByteViewString {
    fn from(s: String) -> Self {
        let bv = ByteView::from_vec(s.into_bytes());
        // SAFETY: We started out with a valid `String`
        Self(SelfCell::new(bv, |s| unsafe {
            std::str::from_utf8_unchecked(&*s)
        }))
    }
}

impl std::ops::Deref for ByteViewString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.get()
    }
}

impl AsRef<[u8]> for ByteViewString {
    fn as_ref(&self) -> &[u8] {
        self.0.get().as_ref()
    }
}

impl PartialEq for ByteViewString {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

/// Provides cached access to source files, with UTF-8 validating contents.
#[derive(Debug, Clone)]
pub struct SourceFilesCache {
    cache: Cacher<FetchFileRequest>,
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
            cache: Cacher::new(cache, shared_cache),
            download_svc,
        }
    }

    /// Retrieves the given [`RemoteFile`] from cache, or fetches it and persists it according
    /// to the provided [`Scope`].
    /// It is possible to avoid using the shared cache using the `use_shared_cache` parameter.
    pub async fn fetch_file(
        &self,
        scope: &Scope,
        file: RemoteFile,
        use_shared_cache: bool,
    ) -> CacheEntry<ByteViewString> {
        let cache_key = CacheKey::from_scoped_file(scope, &file);

        let request = FetchFileRequest {
            file,
            download_svc: Arc::clone(&self.download_svc),
            use_shared_cache,
        };

        self.cache.compute_memoized(request, cache_key).await
    }
}

#[derive(Debug, Clone)]
struct FetchFileRequest {
    file: RemoteFile,
    download_svc: Arc<DownloadService>,
    use_shared_cache: bool,
}

impl CacheItemRequest for FetchFileRequest {
    type Item = ByteViewString;

    const VERSIONS: CacheVersions = SOURCEFILES_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        let fut = async {
            fetch_file(self.download_svc.clone(), self.file.clone(), temp_file).await?;

            let view = ByteView::map_file_ref(temp_file.as_file())?;
            std::str::from_utf8(&view).map_err(|err| CacheError::Malformed(err.to_string()))?;

            Ok(())
        };
        Box::pin(fut)
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        let inner = SelfCell::try_new(data, |s| unsafe {
            std::str::from_utf8(&*s).map_err(|err| CacheError::Malformed(err.to_string()))
        })?;
        Ok(ByteViewString(inner))
    }

    fn use_shared_cache(&self) -> bool {
        self.use_shared_cache
    }
}
