use std::sync::Arc;

use futures::future::BoxFuture;
use url::Url;

use symbolic::common::{ByteView, SelfCell};
use symbolicator_sources::{HttpRemoteFile, RemoteFile};
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
    pub async fn fetch_file(&self, scope: &Scope, file: RemoteFile) -> CacheEntry<ByteViewString> {
        // FIXME: We should probably make it possible to somehow predictably re-fetch
        // files and respect the caching headers from external servers. Right now we
        // keep these files indefinitely. See:
        // <https://github.com/getsentry/symbolicator/issues/1059>
        let cache_key = CacheKey::from_scoped_file(scope, &file);

        let request = FetchFileRequest {
            file,
            download_svc: Arc::clone(&self.download_svc),
        };

        self.cache.compute_memoized(request, cache_key).await
    }

    /// Fetches the file from the given [`Url`] and caches it globally.
    pub async fn fetch_public_url(&self, url: Url) -> CacheEntry<ByteViewString> {
        let scope = Scope::Global;
        self.fetch_scoped_url(&scope, url).await
    }

    /// Fetches the file from the given [`Url`] and caches it according to the given [`Scope`].
    pub async fn fetch_scoped_url(&self, scope: &Scope, url: Url) -> CacheEntry<ByteViewString> {
        let file = HttpRemoteFile::from_url(url);
        self.fetch_file(scope, file.into()).await
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
