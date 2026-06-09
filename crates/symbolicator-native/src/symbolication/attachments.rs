use std::fs::File;
use std::sync::Arc;

use symbolicator_service::download::{DownloadService, fetch_file};
use symbolicator_sources::{HttpRemoteFile, RemoteFile};
use url::Url;

use crate::interface::AttachmentFile;

pub async fn download_attachment(
    download_svc: Arc<DownloadService>,
    file: AttachmentFile,
) -> anyhow::Result<File> {
    let (storage_url, storage_token) = match file {
        AttachmentFile::Local(file) => return Ok(file),
        AttachmentFile::Remote {
            storage_url,
            storage_token,
        } => (storage_url, storage_token),
    };

    let mut http_remote_file = HttpRemoteFile::from_url(Url::parse(&storage_url)?, true);

    if let Some(token) = storage_token {
        http_remote_file = http_remote_file.bearer_auth(&token);
    }

    let mut temp_file = tempfile::NamedTempFile::new()?;

    fetch_file(
        download_svc,
        RemoteFile::Http(http_remote_file),
        &mut temp_file,
    )
    .await?;

    Ok(temp_file.into_file())
}
