use std::io::Seek;
use std::sync::Arc;

use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;

use super::DownloadService;
use super::compression::{DecompressOutcome, maybe_decompress_file};
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
    fetch_file_with_raw_sink(downloader, file_id, temp_file, false)
        .await
        .map(|_| ())
}

/// Like [`fetch_file`], but additionally preserves the upstream-compressed payload (if any)
/// when `want_raw_sink` is `true`.
///
/// Returns a [`DecompressOutcome`] indicating which compression was detected (if any) and,
/// when applicable, a sibling tempfile containing the original compressed bytes.
///
/// This is used to populate the `raw_compressed` cache so the `/proxy` endpoint can serve
/// byte-identical responses for `.pd_` / `.dl_` / `.ex_` requests.
#[tracing::instrument(skip(downloader, temp_file), fields(%file_id))]
pub async fn fetch_file_with_raw_sink(
    downloader: Arc<DownloadService>,
    file_id: RemoteFile,
    temp_file: &mut NamedTempFile,
    want_raw_sink: bool,
) -> CacheContents<DecompressOutcome> {
    downloader
        .download(file_id, temp_file.path().to_owned())
        .await?;
    tracing::trace!("Finished download");

    // Treat decompression errors as malformed files. It is more likely that
    // the error comes from a corrupt file than a local file system error.
    let outcome = maybe_decompress_file(temp_file, want_raw_sink)
        .map_err(|e| CacheError::Malformed(e.to_string()))?;

    temp_file.as_file().rewind()?;
    Ok(outcome)
}
