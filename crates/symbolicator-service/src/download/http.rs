//! Support to download from HTTP sources.

use reqwest::{Client, header};

use symbolicator_sources::HttpRemoteFile;

use crate::{
    caching::{CacheContents, CacheError},
    config::DownloadTimeouts,
};

use super::{Destination, USER_AGENT};

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
        destination: impl Destination,
    ) -> CacheContents {
        let download_url = file_source.url().map_err(|_| CacheError::NotFound)?;

        tracing::debug!("Fetching debug file from `{}`", download_url);

        // Use `self.no_ssl_client` if the source is configured to accept invalid SSL certs
        let builder = if file_source.source.accept_invalid_certs {
            self.no_ssl_client.get(download_url)
        } else {
            self.client.get(download_url)
        };

        super::download_reqwest(
            source_name,
            builder.headers(Self::headers_for_source(file_source)),
            &self.timeouts,
            destination,
        )
        .await
    }

    /// Returns a map of headers that should be used for requests to the given source.
    fn headers_for_source(file_source: &HttpRemoteFile) -> header::HeaderMap {
        let mut headers: header::HeaderMap = file_source
            .source
            .headers
            .0
            .iter()
            .chain(file_source.headers.0.iter())
            .filter_map(|(name, value)| {
                let name = header::HeaderName::from_bytes(name.as_bytes())
                    .inspect_err(|_| tracing::warn!(name, "Invalid header name"))
                    .ok()?;

                let value = header::HeaderValue::from_str(value)
                    .inspect_err(|_| tracing::warn!(value, "Invalid header value"))
                    .ok()?;
                Some((name, value))
            })
            .collect();

        // Set the default user agent if the source config doesn't override it
        if !headers.contains_key(header::USER_AGENT) {
            headers.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(USER_AGENT),
            );
        }

        headers
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    use symbolicator_sources::{
        HttpHeaders, HttpSourceConfig, SourceConfig, SourceId, SourceLocation,
    };

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

    #[test]
    fn test_user_agent() {
        let source_with_user_agent = HttpSourceConfig {
            id: SourceId::new("with-user-agent"),
            url: "http://test.com/".parse().unwrap(),
            headers: HttpHeaders([("User-Agent".to_owned(), "curl/7.72.0".to_owned())].into()),
            files: Default::default(),
            accept_invalid_certs: false,
        };

        let file_with_user_agent =
            HttpRemoteFile::new(Arc::new(source_with_user_agent), SourceLocation::new(""));

        assert_eq!(
            HttpDownloader::headers_for_source(&file_with_user_agent)[header::USER_AGENT],
            "curl/7.72.0"
        );

        let source_without_user_agent = HttpSourceConfig {
            id: SourceId::new("without-user-agent"),
            url: "http://test.com/".parse().unwrap(),
            headers: Default::default(),
            files: Default::default(),
            accept_invalid_certs: false,
        };

        let file_without_user_agent =
            HttpRemoteFile::new(Arc::new(source_without_user_agent), SourceLocation::new(""));

        assert_eq!(
            HttpDownloader::headers_for_source(&file_without_user_agent)[header::USER_AGENT],
            USER_AGENT
        );
    }
}
