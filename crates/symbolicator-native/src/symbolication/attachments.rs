use std::fs::File;

use futures::TryStreamExt;
use symbolicator_service::caching::CacheError;
use symbolicator_service::download::DownloadService;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};

use crate::interface::AttachmentFile;

pub async fn download_attachment(
    download_svc: &DownloadService,
    file: AttachmentFile,
) -> Result<File, CacheError> {
    let (storage_url, storage_token) = match file {
        AttachmentFile::Local(file) => return Ok(file),
        AttachmentFile::Remote {
            storage_url,
            storage_token,
        } => (storage_url, storage_token),
    };

    // TODO: maybe its worth using the actual `DownloadService` instead of straight going to the `trusted_client`.
    // Doing so would in theory allow us to have retries and error report, as well as being able to
    // download files in multiple chunks concurrently, but I don’t think our `objecstore` server currently
    // supports range requests, and those would also mess with streaming decompression.
    // Not to mention that using the `DownloadService` is not that straight forward.
    let mut request = download_svc.trusted_client.get(storage_url);
    if let Some(token) = storage_token {
        request = request.bearer_auth(token);
    }
    let mut stream = request.send().await?.error_for_status()?.bytes_stream();

    let file = tempfile::tempfile()?;
    let mut writer = BufWriter::new(tokio::fs::File::from_std(file));
    while let Some(chunk) = stream.try_next().await? {
        writer.write_all(&chunk).await?;
    }
    writer.flush().await?;
    let mut file = writer.into_inner();
    file.sync_data().await?;

    file.rewind().await?;

    Ok(file.into_std().await)
}
