use std::io::Seek;
use std::sync::Arc;

use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;

use super::DownloadService;
use super::compression::maybe_decompress_file;
use crate::caching::{CacheContents, CacheError};

/// Downloads the gives [`RemoteFile`] and decompresses it.
///
/// This takes a [`NamedTempFile`] to store the resulting file into, and will return a
/// [`NamedTempFile`] back to the caller. This is either the original in case no decompression
/// needs to happen, or a new one in case the downloaded file needs to be decompressed. In that case,
/// a new [`NamedTempFile`] in the same directory will be created and returned.
#[tracing::instrument(skip(downloader, temp_file), fields(%file_id))]
pub async fn fetch_file(
    downloader: Arc<DownloadService>,
    file_id: RemoteFile,
    temp_file: &mut NamedTempFile,
) -> CacheContents {
    downloader
        .download(file_id, temp_file.path().to_owned())
        .await?;
    tracing::trace!("Finished download");

    // Treat decompression errors as malformed files. It is more likely that
    // the error comes from a corrupt file than a local file system error.
    maybe_decompress_file(temp_file).map_err(|e| CacheError::Malformed(e.to_string()))?;

    Ok(temp_file.as_file().rewind()?)
}
