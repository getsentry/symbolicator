use std::fs::File;
use std::sync::Arc;

use symbolicator_service::{
    caching::CacheError,
    download::{DownloadService, fetch_file},
};
use symbolicator_sources::{AttachmentRemoteFile, SentryToken};

use crate::interface::AttachmentFile;

#[tracing::instrument(skip(download_svc))]
pub async fn download_attachment(
    download_svc: Arc<DownloadService>,
    file: AttachmentFile,
) -> Result<File, CacheError> {
    let (storage_url, storage_token) = match file {
        AttachmentFile::Local(file) => return Ok(file),
        AttachmentFile::Remote {
            storage_url,
            storage_token,
        } => (storage_url, storage_token),
    };

    let remote_file = AttachmentRemoteFile {
        url: storage_url,
        token: storage_token.map(SentryToken),
    };

    let mut temp_file = tempfile::NamedTempFile::new()?;

    fetch_file(download_svc, remote_file.into(), &mut temp_file).await?;

    Ok(temp_file.into_file())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use axum::Router;
    use axum::http::{HeaderMap, Uri, header};
    use axum::routing::get;
    use symbolicator_service::config::Config;
    use symbolicator_test::Server;

    use super::*;

    #[tokio::test]
    async fn download_internal_attachment() {
        symbolicator_test::setup();
        let config = Config {
            connect_to_reserved_ips: false,
            ..Default::default()
        };
        let download_svc = DownloadService::new(&config, tokio::runtime::Handle::current());

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
            let attachment = AttachmentFile::Remote {
                storage_url: server.url("/attachment?signature=abc%2F123"),
                storage_token: token,
            };
            let mut file = download_attachment(Arc::clone(&download_svc), attachment)
                .await
                .unwrap();
            let mut contents = String::new();
            file.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "attachment contents");
        }
    }
}
