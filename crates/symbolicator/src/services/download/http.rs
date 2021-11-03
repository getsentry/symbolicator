//! Support to download from HTTP sources.
//!
//! Specifically this supports the [`HttpSourceConfig`] source.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::prelude::*;
use reqwest::{header, Client, StatusCode};
use url::Url;

use super::{
    content_length_timeout, DownloadError, DownloadStatus, RemoteDif, RemoteDifUri, SourceLocation,
    USER_AGENT,
};
use crate::sources::{FileType, HttpSourceConfig};
use crate::types::ObjectId;

/// The HTTP-specific [`RemoteDif`].
#[derive(Debug, Clone)]
pub struct HttpRemoteDif {
    pub source: Arc<HttpSourceConfig>,
    pub location: SourceLocation,
}

impl From<HttpRemoteDif> for RemoteDif {
    fn from(source: HttpRemoteDif) -> Self {
        Self::Http(source)
    }
}

impl HttpRemoteDif {
    pub fn new(source: Arc<HttpSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    pub fn uri(&self) -> RemoteDifUri {
        match self.url() {
            Ok(url) => url.as_ref().into(),
            Err(_) => "".into(),
        }
    }

    /// Returns the URL from which to download this object file.
    pub fn url(&self) -> Result<Url> {
        self.location.to_url(&self.source.url)
    }
}

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
        file_source: HttpRemoteDif,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        let download_url = match file_source.url() {
            Ok(x) => x,
            Err(_) => return Ok(DownloadStatus::NotFound),
        };

        log::debug!("Fetching debug file from {}", download_url);
        let mut builder = self.client.get(download_url.clone());

        for (key, value) in file_source.source.headers.iter() {
            if let Ok(key) = header::HeaderName::from_bytes(key.as_bytes()) {
                builder = builder.header(key, value.as_str());
            }
        }
        let source = RemoteDif::from(file_source);
        let request = builder.header(header::USER_AGENT, USER_AGENT).send();
        let request = tokio::time::timeout(self.connect_timeout, request);
        let request = super::measure_download_time(source.source_metric_key(), request);

        match request.await {
            Ok(Ok(response)) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", download_url);

                    let content_length = response
                        .headers()
                        .get(header::CONTENT_LENGTH)
                        .and_then(|hv| hv.to_str().ok())
                        .and_then(|s| s.parse::<u32>().ok());

                    let timeout =
                        content_length.map(|cl| content_length_timeout(cl, self.streaming_timeout));

                    let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

                    super::download_stream(&source, stream, destination, timeout).await
                } else if matches!(
                    response.status(),
                    StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
                ) {
                    log::debug!("Insufficient permissions to download from {}", download_url);
                    Err(DownloadError::Permissions)
                // If it's a client error, chances are either it's a 404 or it's permission-related.
                } else if response.status().is_client_error() {
                    log::debug!(
                        "Unexpected client error status code from {}: {}",
                        download_url,
                        response.status()
                    );
                    Ok(DownloadStatus::NotFound)
                } else {
                    log::debug!(
                        "Unexpected status code from {}: {}",
                        download_url,
                        response.status()
                    );
                    Err(DownloadError::Rejected(response.status()))
                }
            }
            Ok(Err(e)) => {
                log::debug!("Skipping response from {}: {}", download_url, e);
                Err(DownloadError::Reqwest(e)) // must be wrong type
            }
            Err(_) => {
                // Timeout
                Err(DownloadError::Canceled)
            }
        }
    }

    pub fn list_files(
        &self,
        source: Arc<HttpSourceConfig>,
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
        .map(|loc| HttpRemoteDif::new(source.clone(), loc).into())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::locations::SourceLocation;
    use super::*;

    use crate::sources::SourceConfig;
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
        let file_source = HttpRemoteDif::new(http_source, loc);

        let downloader = HttpDownloader::new(
            Client::new(),
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        let download_status = downloader.download_source(file_source, dest).await.unwrap();

        assert_eq!(download_status, DownloadStatus::Completed);

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
        let file_source = HttpRemoteDif::new(http_source, loc);

        let downloader = HttpDownloader::new(
            Client::new(),
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        let download_status = downloader.download_source(file_source, dest).await.unwrap();

        assert_eq!(download_status, DownloadStatus::NotFound);
    }
}
