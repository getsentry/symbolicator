//! Support to download from the local filesystem.
//!
//! It allows sources to be present on the local filesystem, usually only used for testing.

use std::io;
use std::path::Path;

use tokio::fs;

use symbolicator_sources::FilesystemRemoteFile;

use crate::caching::{CacheEntry, CacheError};

/// Downloader implementation that supports the filesystem source.
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
    ) -> CacheEntry {
        // All file I/O in this function is blocking!
        let abspath = file_source.path();
        tracing::debug!("Fetching debug file from {:?}", abspath);

        fs::copy(abspath, dest)
            .await
            .map(|_| ())
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => CacheError::NotFound,
                _ => e.into(),
            })
    }
}
