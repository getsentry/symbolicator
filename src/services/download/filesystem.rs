//! Support to download from the local filesystem.
//!
//! Specifically this supports the [`FilesystemSourceConfig`] source.  It allows
//! sources to be present on the local filesystem, usually only used for
//! testing.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use super::{DownloadError, DownloadStatus};
use crate::sources::{
    FileType, FilesystemObjectFileSource, FilesystemSourceConfig, ObjectFileSource,
};
use crate::types::ObjectId;

/// Downloader implementation that supports the [`FilesystemSourceConfig`] source.
#[derive(Debug)]
pub struct FilesystemDownloader {}

impl FilesystemDownloader {
    pub fn new() -> Self {
        Self {}
    }

    /// Download from a filesystem source.
    pub fn download_source(
        &self,
        file_source: FilesystemObjectFileSource,
        dest: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        // All file I/O in this function is blocking!
        let abspath = file_source.path();
        log::debug!("Fetching debug file from {:?}", abspath);
        match fs::copy(abspath, dest) {
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
        filetypes: &'static [FileType],
        object_id: ObjectId,
    ) -> Vec<ObjectFileSource> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id: &object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| FilesystemObjectFileSource::new(source.clone(), loc).into())
        .collect()
    }
}
