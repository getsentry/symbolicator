//! Support to download from the local filesystem.
//!
//! It allows sources to be present on the local filesystem, usually only used for testing.

use std::io;

use tokio::fs::File;

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
        file_source: &FilesystemRemoteFile,
        destination: &mut File,
    ) -> CacheEntry {
        let path = file_source.path();
        tracing::debug!("Fetching debug file from {:?}", path);

        let mut file = File::open(path).await.map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => CacheError::NotFound,
            _ => e.into(),
        })?;
        tokio::io::copy(&mut file, destination).await?;
        Ok(())
    }
}
