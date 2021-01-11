//! Support to download from HTTP sources.
//!
//! Specifically this supports the [`HttpSourceConfig`] source.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix_web::http::header;
use actix_web::{client, HttpMessage};
use futures::compat::Stream01CompatExt;
use futures::prelude::*;
use url::Url;

use super::{DownloadError, DownloadStatus, USER_AGENT};
use crate::sources::{FileType, HttpObjectFileSource, HttpSourceConfig, ObjectFileSource};
use crate::types::ObjectId;
use crate::utils::futures as future_utils;
use crate::utils::http;

/// The maximum number of redirects permitted by a remote symbol server.
const MAX_HTTP_REDIRECTS: usize = 10;

/// Downloader implementation that supports the [`HttpSourceConfig`] source.
#[derive(Debug)]
pub struct HttpDownloader {}

impl HttpDownloader {
    pub fn new() -> Self {
        Self {}
    }

    /// Start a request to the web server.
    ///
    /// This initiates the request, when the future resolves the headers will have been
    /// retrieved but not the entire payload.
    async fn start_request(
        &self,
        source: &HttpSourceConfig,
        url: Url,
    ) -> Result<client::ClientResponse, client::SendRequestError> {
        http::follow_redirects(url, MAX_HTTP_REDIRECTS, move |url| {
            let mut builder = client::get(url);
            for (key, value) in source.headers.iter() {
                if let Ok(key) = header::HeaderName::from_bytes(key.as_bytes()) {
                    builder.header(key, value.as_str());
                }
            }
            builder.header(header::USER_AGENT, USER_AGENT);
            // This timeout is for the entire HTTP download *including* the response stream
            // itself, in contrast to what the Actix-Web docs say. We have tested this
            // manually.
            //
            // The intent is to disable the timeout entirely, but there is no API for that.
            builder.timeout(Duration::from_secs(9999));
            builder.finish()
        })
        .await
    }

    pub async fn download_source(
        &self,
        file_source: HttpObjectFileSource,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let download_url = match file_source.url() {
            Ok(x) => x,
            Err(_) => return Ok(DownloadStatus::NotFound),
        };
        log::debug!("Fetching debug file from {}", download_url);
        let response =
            future_utils::retry(|| self.start_request(&file_source.source, download_url.clone()))
                .await;

        match response {
            Ok(response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", download_url);
                    let stream = response
                        .payload()
                        .compat()
                        .map(|i| i.map_err(DownloadError::stream));
                    super::download_stream(file_source.into(), stream, destination).await
                } else {
                    log::trace!(
                        "Unexpected status code from {}: {}",
                        download_url,
                        response.status()
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
        .map(|loc| HttpObjectFileSource::new(source.clone(), loc).into())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sources::{SourceConfig, SourceLocation};
    use crate::test;

    #[test]
    fn test_download_source() {
        test::setup();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path().to_owned();

        let (_srv, source) = test::symbol_server();
        let http_source = match source {
            SourceConfig::Http(source) => source,
            _ => panic!("unexpected source"),
        };
        let loc = SourceLocation::new("hello.txt");
        let file_source = HttpObjectFileSource::new(http_source, loc);

        let downloader = HttpDownloader::new();
        let ret = test::block_fn(|| downloader.download_source(file_source, dest.clone()));
        assert_eq!(ret.unwrap(), DownloadStatus::Completed);
        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n");
    }

    #[test]
    fn test_download_source_missing() {
        test::setup();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path().to_owned();

        let (_srv, source) = test::symbol_server();
        let http_source = match source {
            SourceConfig::Http(source) => source,
            _ => panic!("unexpected source"),
        };
        let loc = SourceLocation::new("i-do-not-exist");
        let file_source = HttpObjectFileSource::new(http_source, loc);

        let downloader = HttpDownloader::new();
        let ret = test::block_fn(|| downloader.download_source(file_source, dest.clone()));
        assert_eq!(ret.unwrap(), DownloadStatus::NotFound);
    }
}
