//! Support to download from the local filesystem.
//!
//! Specifically this supports the [`FilesystemSourceConfig`] source.  It allows
//! sources to be present on the local filesystem, usually only used for
//! testing.
//!
//! [`FilesystemSourceConfig`]: ../../../sources/struct.S3SourceConfig.html

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use super::types::{DownloadError, DownloadErrorKind, DownloadStatus};
use crate::sources::{FilesystemSourceConfig, SourceLocation};

/// Download from a filesystem source.
///
/// The file is saved in `dest` and will be written incrementally to it.  Once
/// this successfully returns the file will be completely written.  If this
/// completes with an error return any data in the file is to be considered
/// garbage.
pub fn download_source(
    source: Arc<FilesystemSourceConfig>,
    location: SourceLocation,
    dest: PathBuf,
) -> Result<DownloadStatus, DownloadError> {
    // All file I/O in this function is blocking!
    let abspath = source.join_loc(&location);
    log::debug!("Fetching debug file from {:?}", abspath);
    match fs::copy(abspath, dest) {
        Ok(_) => Ok(DownloadStatus::Completed),
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound => Ok(DownloadStatus::NotFound),
            _ => Err(DownloadError::from(DownloadErrorKind::Io)),
        },
    }
}
