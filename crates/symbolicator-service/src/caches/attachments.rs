use crate::{
    caches::versions::ATTACHMENTS_CACHE_VERSIONS,
    caching::{Cache, CacheContents, CacheItemRequest, CacheKey, Cacher, SharedCacheService},
    download::{DownloadService, fetch_file},
    types::Scope,
    utils::fs,
};
use std::sync::Arc;
use symbolic::common::ByteView;
use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;
use tokio::sync::OnceCell;

#[derive(Clone)]
struct AttachmentDownload {
    file_source: RemoteFile,
    download_svc: Arc<DownloadService>,
}

impl CacheItemRequest for AttachmentDownload {
    type Item = ByteView<'static>;

    const VERSIONS: crate::caches::CacheVersions = ATTACHMENTS_CACHE_VERSIONS;

    fn compute<'a>(
        &'a self,
        temp_file: &'a mut NamedTempFile,
    ) -> futures::prelude::future::BoxFuture<'a, crate::caching::CacheContents> {
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
    ) -> crate::caching::CacheContents<Self::Item> {
        Ok(data)
    }

    fn use_shared_cache(&self) -> bool {
        false
    }
}

/// A service that exposes downloading and caching attachments.
#[derive(Debug)]
pub struct AttachmentsService {
    download_svc: Arc<DownloadService>,
    attachments_cache: Arc<Cacher<AttachmentDownload>>,
}

impl AttachmentsService {
    /// Constructs a new AttachmentService.
    pub fn new(
        attachments_cache: Cache,
        shared_cache: Arc<OnceCell<SharedCacheService>>,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        let attachments_cache = Cacher::new(attachments_cache, shared_cache).into();
        Self {
            download_svc,
            attachments_cache,
        }
    }

    /// Downloads and caches the specified file under the provided scope.
    #[tracing::instrument(skip(self))]
    pub async fn download(
        &self,
        scope: &Scope,
        file_source: RemoteFile,
        use_cache: bool,
    ) -> CacheContents<ByteView<'static>> {
        if !use_cache {
            let mut temp_file = fs::tempfile(self.download_svc.tmp_dir.as_deref())?;

            fetch_file(self.download_svc.clone(), file_source, &mut temp_file).await?;

            let f = temp_file.into_file();

            let bv = ByteView::map_file(f)?;
            return Ok(bv);
        }

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

#[cfg(test)]
mod tests {
    use std::io::Read;

    use axum::Router;
    use axum::http::{HeaderMap, Uri, header};
    use axum::routing::get;
    use symbolicator_sources::SentryToken;
    use symbolicator_test::Server;

    use crate::caching::Caches;
    use crate::config::Config;

    use super::*;

    #[tokio::test]
    async fn download_internal_attachment() {
        symbolicator_test::setup();
        let config = Config {
            connect_to_reserved_ips: false,
            ..Default::default()
        };
        let download_svc = DownloadService::new(&config, tokio::runtime::Handle::current());
        let runtime = tokio::runtime::Handle::current();
        let caches = Caches::from_config(&config).unwrap();
        let shared_cache = SharedCacheService::new(config.shared_cache.clone(), runtime);

        let attachments_svc =
            AttachmentsService::new(caches.attachments, shared_cache, download_svc.clone());

        for token in [None, Some("attachment-token".to_owned())] {
            let expected_token = token.clone();
            let router = Router::new().route(
                "/attachment",
                get(move |headers: HeaderMap, uri: Uri| async move {
                    assert_eq!(uri.query(), Some("signature=abc%2F123"));
                    let expected_auth = expected_token.map(|token| format!("Bearer {token}"));
                    assert_eq!(
                        headers
                            .get(header::AUTHORIZATION)
                            .map(|h| h.to_str().unwrap()),
                        expected_auth.as_deref(),
                    );
                    (
                        [(header::CONTENT_ENCODING, "zstd")],
                        zstd::bulk::compress(b"attachment contents", 0).unwrap(),
                    )
                }),
            );
            let server = Server::with_router(router);
            let file_source = RemoteFile::Attachment(symbolicator_sources::AttachmentRemoteFile {
                url: server.url("/attachment?signature=abc%2F123"),
                token: token.map(SentryToken),
            });

            let view = attachments_svc
                .download(&Scope::Global, file_source, false)
                .await
                .unwrap();
            let mut contents = String::new();
            view.as_slice().read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "attachment contents");
        }
    }
}
