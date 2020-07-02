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

use super::{DownloadError, DownloadErrorKind, DownloadStatus};
use crate::sources::{FileType, FilesystemSourceConfig, SourceFileId, SourceLocation};
use crate::types::ObjectId;

/// Download from a filesystem source.
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

pub fn list_files(
    source: Arc<FilesystemSourceConfig>,
    filetypes: &'static [FileType],
    object_id: ObjectId,
) -> Vec<SourceFileId> {
    super::SourceLocationIter {
        filetypes: filetypes.iter(),
        filters: &source.files.filters,
        object_id: &object_id,
        layout: source.files.layout,
        next: Vec::new(),
    }
    .map(|loc| SourceFileId::Filesystem(source.clone(), loc))
    .collect()
}
