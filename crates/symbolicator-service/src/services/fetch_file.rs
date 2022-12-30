use std::io::Seek;
use std::sync::Arc;

use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;

use crate::cache::{CacheEntry, CacheError};
use crate::services::download::DownloadService;
use crate::utils::compression::maybe_decompress_file;

/// Downloads the gives [`RemoteFile`] and decompresses it.
///
/// This takes a [`NamedTempFile`] to store the resulting file into, and will return a
/// [`NamedTempFile`] back to the caller. This is either the original in case no decompression
/// needs to happen, or a new one in case the downloaded file needs to be decompressed. In that case,
/// a new [`NamedTempFile`] in the same directory will be created and returned.
#[tracing::instrument(skip(downloader, temp_file))]
pub async fn fetch_file(
    downloader: Arc<DownloadService>,
    file_id: RemoteFile,
    temp_file: NamedTempFile,
) -> CacheEntry<NamedTempFile> {
    let temp_file = downloader.download(file_id, temp_file).await?;
    tracing::trace!("Finished download");

    // Treat decompression errors as malformed files. It is more likely that
    // the error comes from a corrupt file than a local file system error.
    let temp_file =
        maybe_decompress_file(temp_file).map_err(|e| CacheError::Malformed(e.to_string()))?;
    temp_file.as_file().rewind()?;
    Ok(temp_file)
}
