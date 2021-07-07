//! Support to download from HTTP sources.
//!
//! Specifically this supports the [`HttpSourceConfig`] source.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use futures::prelude::*;
use reqwest::{header, Client};
use url::Url;

use super::{DownloadError, DownloadStatus, RemoteDif, RemoteDifUri, SourceLocation, USER_AGENT};
use crate::sources::{FileType, HttpSourceConfig};
use crate::types::ObjectId;
use crate::utils::futures as future_utils;

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
}

impl HttpDownloader {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub async fn download_source(
        &self,
        file_source: HttpRemoteDif,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let retries = future_utils::retry(|| {
            self.download_source_once(file_source.clone(), destination.clone())
        });

        let download_url = file_source.url().ok();
        match retries.await {
            Ok(status) => {
                log::debug!("Fetched debug file from {:?}: {:?}", download_url, status);
                Ok(status)
            }
            Err(err) => {
                log::debug!(
                    "Failed to fetch debug file from {:?}: {}",
                    download_url,
                    err
                );
                Err(err)
            }
        }
    }

    async fn download_source_once(
        &self,
        file_source: HttpRemoteDif,
        destination: PathBuf,
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
        let request = future_utils::measure_source_download(
            "service.download.download_source",
            source.source_metric_key(),
            future_utils::m::result,
            request,
        );

        match request.await {
            Ok(request) => {
                if request.status().is_success() {
                    log::trace!("Success hitting {}", download_url);
                    let stream = request.bytes_stream().map_err(DownloadError::Reqwest);
                    super::download_stream(source, stream, destination).await
                } else {
                    log::trace!(
                        "Unexpected status code from {}: {}",
                        download_url,
                        request.status()
                    );
                    Ok(DownloadStatus::NotFound)
                }
            }
            Err(e) => {
                log::trace!("Skipping response from {}: {}", download_url, e);
                Ok(DownloadStatus::NotFound) // must be wrong type
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
        let dest = tmpfile.path().to_owned();

        let (_srv, source) = test::symbol_server();
        let http_source = match source {
            SourceConfig::Http(source) => source,
            _ => panic!("unexpected source"),
        };
        let loc = SourceLocation::new("hello.txt");
        let file_source = HttpRemoteDif::new(http_source, loc);

        let downloader = HttpDownloader::new(Client::new());
        let download_status = downloader
            .download_source(file_source, dest.clone())
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::Completed);

        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n");
    }

    #[tokio::test]
    async fn test_download_source_missing() {
        test::setup();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path().to_owned();

        let (_srv, source) = test::symbol_server();
        let http_source = match source {
            SourceConfig::Http(source) => source,
            _ => panic!("unexpected source"),
        };
        let loc = SourceLocation::new("i-do-not-exist");
        let file_source = HttpRemoteDif::new(http_source, loc);

        let downloader = HttpDownloader::new(Client::new());
        let download_status = downloader.download_source(file_source, dest).await.unwrap();

        assert_eq!(download_status, DownloadStatus::NotFound);
    }
}
