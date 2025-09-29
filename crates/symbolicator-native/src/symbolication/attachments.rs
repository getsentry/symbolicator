use std::fs::File;
use std::pin::pin;

use futures::TryStreamExt;
use symbolicator_service::download::DownloadService;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::io::StreamReader;

use crate::interface::AttachmentFile;

pub async fn download_attachment(
    download_svc: &DownloadService,
    file: AttachmentFile,
) -> anyhow::Result<File> {
    let storage_url = match file {
        AttachmentFile::Local(file) => return Ok(file),
        AttachmentFile::Remote(url) => url,
    };

    // TODO: maybe its worth using the actual `DownloadService` instead of straight going to the `trusted_client`.
    // Doing so would in theory allow us to have retries and error report, as well as being able to
    // download files in multiple chunks concurrently, but I don’t think our `objecstore` server currently
    // supports range requests, and those would also mess with streaming decompression.
    // Not to mention that using the `DownloadService` is not that straight forward.
    let stream = download_svc
        .trusted_client
        .get(storage_url)
        .send()
        .await?
        .error_for_status()?
        .bytes_stream()
        .map_err(std::io::Error::other);
    let mut reader = pin!(StreamReader::new(stream));

    let file = tempfile::tempfile()?;
    let mut writer = BufWriter::new(tokio::fs::File::from_std(file));
    tokio::io::copy(&mut reader, &mut writer).await?;
    writer.flush().await?;
    let file = writer.into_inner();
    file.sync_data().await?;

    Ok(file.into_std().await)
}
