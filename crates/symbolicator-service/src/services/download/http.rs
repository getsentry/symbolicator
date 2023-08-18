//! Support to download from HTTP sources.

use std::path::Path;

use reqwest::{header, Client};

use symbolicator_sources::{HttpRemoteFile, RemoteFile};

use crate::caching::{CacheEntry, CacheError};
use crate::utils::http::DownloadTimeouts;

use super::USER_AGENT;

/// Downloader implementation that supports the HTTP source.
#[derive(Debug)]
pub struct HttpDownloader {
    client: Client,
    timeouts: DownloadTimeouts,
}

impl HttpDownloader {
    pub fn new(client: Client, timeouts: DownloadTimeouts) -> Self {
        Self { client, timeouts }
    }

    /// Downloads a source hosted on an HTTP server.
    pub async fn download_source(
        &self,
        file_source: HttpRemoteFile,
        destination: &Path,
    ) -> CacheEntry {
        let download_url = file_source.url().map_err(|_| CacheError::NotFound)?;

        tracing::debug!("Fetching debug file from `{}`", download_url);
        let mut builder = self.client.get(download_url);

        let headers = file_source
            .source
            .headers
            .iter()
            .chain(file_source.headers.iter());
        for (key, value) in headers {
            if let Ok(key) = header::HeaderName::from_bytes(key.as_bytes()) {
                builder = builder.header(key, value.as_str());
            }
        }

        let request = builder.header(header::USER_AGENT, USER_AGENT);

        let source = RemoteFile::from(file_source);
        let mut destination = tokio::fs::File::create(destination).await?;
        super::download_reqwest(
            source.source_metric_key(),
            request,
            &self.timeouts,
            &mut destination,
        )
        .await
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

        let downloader = HttpDownloader::new(Client::new(), Default::default());
        let download_status = downloader.download_source(file_source, dest).await;

        assert!(download_status.is_ok());

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

        let downloader = HttpDownloader::new(Client::new(), Default::default());
        let download_status = downloader.download_source(file_source, dest).await;

        assert_eq!(download_status, Err(CacheError::NotFound));
    }
}
