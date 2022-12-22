use std::io::Seek;
use std::sync::Arc;

use symbolicator_sources::RemoteFile;
use tempfile::NamedTempFile;

use crate::cache::{CacheEntry, CacheError};
use crate::services::download::{DownloadError, DownloadService, DownloadStatus};
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
    // FIXME(swatinem): Ideally, the downloader would just give us a `CacheEntry<NamedTempFile>` directly.
    let temp_file = match downloader.download(file_id, temp_file).await {
        Ok(DownloadStatus::NotFound) => {
            tracing::debug!("File not found");
            return Err(CacheError::NotFound);
        }
        Ok(DownloadStatus::PermissionDenied) => {
            // FIXME: this is really unreachable as the downloader converts these already
            return Err(CacheError::PermissionDenied("".into()));
        }

        Err(e) => {
            // We want to error-log "interesting" download errors so we can look them up
            // in our internal sentry. We downgrade to debug-log for unactionable
            // permissions errors. Since this function does a fresh download, it will never
            // hit `CachedError`, but listing it for completeness is not a bad idea either.
            let stderr: &dyn std::error::Error = &e;
            match e {
                DownloadError::Permissions | DownloadError::CachedError(_) => {
                    tracing::debug!(stderr, "Error while downloading file")
                }
                _ => tracing::error!(stderr, "Error while downloading file"),
            }

            return Err(CacheError::from(e));
        }

        Ok(DownloadStatus::Completed(temp_file)) => temp_file,
    };
    tracing::trace!("Finished download");

    // Treat decompression errors as malformed files. It is more likely that
    // the error comes from a corrupt file than a local file system error.
    let temp_file =
        maybe_decompress_file(temp_file).map_err(|e| CacheError::Malformed(e.to_string()))?;
    temp_file.as_file().rewind()?;
    Ok(temp_file)
}
