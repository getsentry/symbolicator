//! Support to download from the local filesystem.
//!
//! Specifically this supports the [`FilesystemSourceConfig`] source.  It allows
//! sources to be present on the local filesystem, usually only used for
//! testing.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::fs;

use super::locations::SourceLocation;
use super::{DownloadError, DownloadStatus, RemoteDif, RemoteDifUri};
use crate::sources::{FileType, FilesystemSourceConfig};
use crate::types::ObjectId;

/// Filesystem-specific [`RemoteDif`].
#[derive(Debug, Clone)]
pub struct FilesystemRemoteDif {
    pub source: Arc<FilesystemSourceConfig>,
    pub location: SourceLocation,
}

impl From<FilesystemRemoteDif> for RemoteDif {
    fn from(source: FilesystemRemoteDif) -> Self {
        Self::Filesystem(source)
    }
}

impl FilesystemRemoteDif {
    pub fn new(source: Arc<FilesystemSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the path from which to fetch this object file.
    pub fn path(&self) -> PathBuf {
        self.source.path.join(&self.location.path())
    }

    /// Returns the `file://` URI from which to fetch this object file.
    ///
    /// This is a quick-and-dirty approximation, not fully RFC8089-compliant.  E.g. we do
    /// not provide a hostname nor percent-encode.  Use this only for diagnostics and use
    /// [`FilesystemRemoteDif::path`] if the actual file location is needed.
    pub fn uri(&self) -> RemoteDifUri {
        format!("file:///{}", self.path().display()).into()
    }
}

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
        file_source: FilesystemRemoteDif,
        dest: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        // All file I/O in this function is blocking!
        let abspath = file_source.path();
        log::debug!("Fetching debug file from {:?}", abspath);
        match fs::copy(abspath, dest).await {
            Ok(_) => Ok(DownloadStatus::Completed),
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
        object_id: ObjectId,
    ) -> Vec<RemoteDif> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id: &object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| FilesystemRemoteDif::new(source.clone(), loc).into())
        .collect()
    }
}
