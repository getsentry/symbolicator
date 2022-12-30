//! Support to download from HTTP sources.
//!
//! Specifically this supports the [`HttpSourceConfig`] source.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{header, Client};

use symbolicator_sources::{FileType, HttpRemoteFile, HttpSourceConfig, ObjectId, RemoteFile};
use tokio::fs::File;

use crate::cache::{CacheEntry, CacheError};

use super::USER_AGENT;

/// Downloader implementation that supports the [`HttpSourceConfig`] source.
#[derive(Debug)]
pub struct HttpDownloader {
    client: Client,
    connect_timeout: Duration,
    streaming_timeout: Duration,
}

impl HttpDownloader {
    pub fn new(client: Client, connect_timeout: Duration, streaming_timeout: Duration) -> Self {
        Self {
            client,
            connect_timeout,
            streaming_timeout,
        }
    }

    /// Downloads a source hosted on an HTTP server.
    pub async fn download_source(
        &self,
        file_source: HttpRemoteFile,
        file: &mut File,
    ) -> CacheEntry {
        let download_url = file_source.url().map_err(|_| CacheError::NotFound)?;

        tracing::debug!("Fetching debug file from {}", download_url);
        let mut builder = self.client.get(download_url.clone());

        for (key, value) in file_source.source.headers.iter() {
            if let Ok(key) = header::HeaderName::from_bytes(key.as_bytes()) {
                builder = builder.header(key, value.as_str());
            }
        }
        let source = RemoteFile::from(file_source);
        let request = builder.header(header::USER_AGENT, USER_AGENT);

        super::download_reqwest(
            &source,
            request,
            self.connect_timeout,
            self.streaming_timeout,
            file,
        )
        .await
    }

    pub fn list_files(
        &self,
        source: Arc<HttpSourceConfig>,
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
        .map(|loc| HttpRemoteFile::new(source.clone(), loc).into())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Read;

    use symbolicator_sources::{SourceConfig, SourceLocation};
    use tempfile::tempfile;

    use crate::test;

    #[tokio::test]
    async fn test_download_source() {
        test::setup();

        let file = File::from_std(tempfile().unwrap());

        let (_srv, source) = test::symbol_server();
        let http_source = match source {
            SourceConfig::Http(source) => source,
            _ => panic!("unexpected source"),
        };
        let loc = SourceLocation::new("hello.txt");
        let file_source = HttpRemoteFile::new(http_source, loc);

        let downloader = HttpDownloader::new(
            Client::new(),
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        let result = downloader.download_source(file_source, &mut file).await;

        assert!(result.is_ok());

        let mut content = String::new();
        let file = file.into_std().await;
        file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "hello world\n");
    }

    #[tokio::test]
    async fn test_download_source_missing() {
        test::setup();

        let file = File::from_std(tempfile().unwrap());

        let (_srv, source) = test::symbol_server();
        let http_source = match source {
            SourceConfig::Http(source) => source,
            _ => panic!("unexpected source"),
        };
        let loc = SourceLocation::new("i-do-not-exist");
        let file_source = HttpRemoteFile::new(http_source, loc);

        let downloader = HttpDownloader::new(
            Client::new(),
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        let result = downloader.download_source(file_source, &mut file).await;

        assert_eq!(result, Err(CacheError::NotFound));
    }
}
