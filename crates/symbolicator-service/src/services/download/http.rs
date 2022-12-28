//! Support to download from HTTP sources.
//!
//! Specifically this supports the [`HttpSourceConfig`] source.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::prelude::*;
use reqwest::{header, Client, StatusCode};

use symbolicator_sources::{FileType, HttpRemoteFile, HttpSourceConfig, ObjectId, RemoteFile};

use super::{content_length_timeout, DownloadError, DownloadStatus, USER_AGENT};

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
    ///
    /// # Directly thrown errors
    /// - [`DownloadError::Reqwest`]
    /// - [`DownloadError::Rejected`]
    /// - [`DownloadError::Canceled`]
    pub async fn download_source(
        &self,
        file_source: HttpRemoteFile,
        destination: &Path,
    ) -> Result<DownloadStatus<()>, DownloadError> {
        let download_url = match file_source.url() {
            Ok(x) => x,
            Err(_) => return Ok(DownloadStatus::NotFound),
        };

        tracing::debug!("Fetching debug file from {}", download_url);
        let mut builder = self.client.get(download_url.clone());

        for (key, value) in file_source.source.headers.iter() {
            if let Ok(key) = header::HeaderName::from_bytes(key.as_bytes()) {
                builder = builder.header(key, value.as_str());
            }
        }
        let source = RemoteFile::from(file_source);
        let request = builder.header(header::USER_AGENT, USER_AGENT).send();
        let request = tokio::time::timeout(self.connect_timeout, request);
        let request = super::measure_download_time(source.source_metric_key(), request);

        let response = request
            .await
            .map_err(|_| DownloadError::Canceled)? // Timeout
            .map_err(|e| {
                tracing::debug!("Skipping response from {}: {}", download_url, e);
                DownloadError::Reqwest(e)
            })?;

        if response.status().is_success() {
            tracing::trace!("Success hitting {}", download_url);

            let content_length = response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|hv| hv.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok());

            let timeout =
                content_length.map(|cl| content_length_timeout(cl, self.streaming_timeout));

            let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

            super::download_stream(&source, stream, destination, timeout).await
        } else if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
        ) {
            tracing::debug!("Insufficient permissions to download from {}", download_url);
            Ok(DownloadStatus::PermissionDenied)
        // If it's a client error, chances are either it's a 404 or it's permission-related.
        } else if response.status().is_client_error() {
            tracing::debug!(
                "Unexpected client error status code from {}: {}",
                download_url,
                response.status()
            );
            Ok(DownloadStatus::NotFound)
        } else {
            tracing::debug!(
                "Unexpected status code from {}: {}",
                download_url,
                response.status()
            );
            Err(DownloadError::Rejected(response.status()))
        }
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

    use symbolicator_sources::{SourceConfig, SourceLocation};

    use crate::test;

    #[tokio::test]
    async fn test_download_source() {
        test::setup();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path();

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
        let download_status = downloader.download_source(file_source, dest).await.unwrap();

        assert!(matches!(download_status, DownloadStatus::Completed(_)));

        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n");
    }

    #[tokio::test]
    async fn test_download_source_missing() {
        test::setup();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path();

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
        let download_status = downloader.download_source(file_source, dest).await.unwrap();

        assert!(matches!(download_status, DownloadStatus::NotFound));
    }
}
