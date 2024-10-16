//! Support to download from HTTP sources.

use reqwest::{header, Client};

use symbolicator_sources::HttpRemoteFile;
use tokio::fs::File;

use crate::caching::{CacheEntry, CacheError};
use crate::utils::http::DownloadTimeouts;

use super::USER_AGENT;

/// Downloader implementation that supports the HTTP source.
#[derive(Debug)]
pub struct HttpDownloader {
    client: Client,
    no_ssl_client: Client,
    timeouts: DownloadTimeouts,
}

impl HttpDownloader {
    pub fn new(client: Client, no_ssl_client: Client, timeouts: DownloadTimeouts) -> Self {
        Self {
            client,
            no_ssl_client,
            timeouts,
        }
    }

    /// Downloads a source hosted on an HTTP server.
    pub async fn download_source(
        &self,
        source_name: &str,
        file_source: &HttpRemoteFile,
        destination: &mut File,
    ) -> CacheEntry {
        let download_url = file_source.url().map_err(|_| CacheError::NotFound)?;

        tracing::debug!("Fetching debug file from `{}`", download_url);

        // Use `self.no_ssl_client` if the source is configured to accept invalid SSL certs
        let mut builder = if file_source.source.accept_invalid_certs {
            self.no_ssl_client.get(download_url)
        } else {
            self.client.get(download_url)
        };

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
        builder = builder.header(header::USER_AGENT, USER_AGENT);

        super::download_reqwest(source_name, builder, &self.timeouts, destination).await
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

        let downloader = HttpDownloader::new(Client::new(), Client::new(), Default::default());
        let mut destination = tokio::fs::File::create(&dest).await.unwrap();
        let download_status = downloader
            .download_source("", &file_source, &mut destination)
            .await;

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

        let downloader = HttpDownloader::new(Client::new(), Client::new(), Default::default());
        let mut destination = tokio::fs::File::create(&dest).await.unwrap();
        let download_status = downloader
            .download_source("", &file_source, &mut destination)
            .await;

        assert_eq!(download_status, Err(CacheError::NotFound));
    }

    #[tokio::test]
    async fn test_download_azure_file() {
        test::setup();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path();

        let file_source =
            HttpRemoteFile::from_url("https://dev.azure.com/foo/bar.cs".parse().unwrap(), true);

        let restricted_client = crate::utils::http::create_client(&Default::default(), true, false);
        let no_ssl_client = crate::utils::http::create_client(&Default::default(), true, true);

        let downloader = HttpDownloader::new(restricted_client, no_ssl_client, Default::default());
        let mut destination = tokio::fs::File::create(&dest).await.unwrap();
        let download_status = downloader
            .download_source("", &file_source, &mut destination)
            .await;

        assert_eq!(
            download_status,
            Err(CacheError::PermissionDenied(
                "Potential login page detected".into()
            ))
        );
    }
}
