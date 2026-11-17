use std::sync::Arc;
use symbolic::common::ByteView;
use symbolicator_service::{
    caches::versions::ATTACHMENTS_CACHE_VERSIONS,
    caching::{CacheContents, CacheItemRequest, CacheKey, Cacher},
    download::{DownloadService, fetch_file},
    services::SharedServices,
    types::Scope,
};
use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;

#[derive(Clone)]
struct AttachmentDownload {
    file_source: RemoteFile,
    download_svc: Arc<DownloadService>,
}

impl CacheItemRequest for AttachmentDownload {
    type Item = ByteView<'static>;

    const VERSIONS: symbolicator_service::caches::CacheVersions = ATTACHMENTS_CACHE_VERSIONS;

    fn compute<'a>(
        &'a self,
        temp_file: &'a mut NamedTempFile,
    ) -> futures::prelude::future::BoxFuture<'a, symbolicator_service::caching::CacheContents> {
        let fut = async {
            fetch_file(
                self.download_svc.clone(),
                self.file_source.clone(),
                temp_file,
            )
            .await?;

            Ok(())
        };
        Box::pin(fut)
    }

    fn load(
        &self,
        data: symbolic::common::ByteView<'static>,
    ) -> symbolicator_service::caching::CacheContents<Self::Item> {
        Ok(data)
    }

    fn use_shared_cache(&self) -> bool {
        false
    }
}

/// A service that exposes downloading and caching attachments.
pub struct AttachmentsService {
    download_svc: Arc<DownloadService>,
    attachments_cache: Arc<Cacher<AttachmentDownload>>,
}

impl AttachmentsService {
    /// Constructs a new AttachmentService.
    pub fn new(shared_services: &SharedServices) -> Self {
        let attachments_cache = Cacher::new(
            shared_services.caches.attachments.clone(),
            shared_services.shared_cache.clone(),
        )
        .into();
        Self {
            download_svc: shared_services.download_svc.clone(),
            attachments_cache,
        }
    }

    /// Downloads and caches the specified file under the provided scope.
    pub async fn download(
        &self,
        scope: &Scope,
        file_source: RemoteFile,
    ) -> CacheContents<ByteView<'static>> {
        let cache_key = CacheKey::from_scoped_file(scope, &file_source);
        let request = AttachmentDownload {
            file_source,
            download_svc: self.download_svc.clone(),
        };

        self.attachments_cache
            .compute_memoized(request, cache_key)
            .await
            .into_contents()
    }
}
