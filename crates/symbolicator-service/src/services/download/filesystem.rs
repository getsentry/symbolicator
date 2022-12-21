//! Support to download from the local filesystem.
//!
//! Specifically this supports the [`FilesystemSourceConfig`] source.  It allows
//! sources to be present on the local filesystem, usually only used for
//! testing.

use std::io;
use std::path::Path;
use std::sync::Arc;

use tokio::fs;

use symbolicator_sources::{
    FileType, FilesystemRemoteFile, FilesystemSourceConfig, ObjectId, RemoteFile,
};

use super::{DownloadError, DownloadStatus};

/// Downloader implementation that supports the [`FilesystemSourceConfig`] source.
#[derive(Debug)]
pub struct FilesystemDownloader {}

impl FilesystemDownloader {
    pub fn new() -> Self {
        Self {}
    }

    /// Download from a filesystem source.
    pub async fn download_source(
        &self,
        file_source: FilesystemRemoteFile,
        dest: &Path,
    ) -> Result<DownloadStatus<()>, DownloadError> {
        // All file I/O in this function is blocking!
        let abspath = file_source.path();
        tracing::debug!("Fetching debug file from {:?}", abspath);
        match fs::copy(abspath, dest).await {
            Ok(_) => Ok(DownloadStatus::Completed(())),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Ok(DownloadStatus::NotFound),
                _ => Err(DownloadError::Io(e)),
            },
        }
    }

    pub fn list_files(
        &self,
        source: Arc<FilesystemSourceConfig>,
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> Vec<RemoteFile> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| FilesystemRemoteFile::new(source.clone(), loc).into())
        .collect()
    }
}
