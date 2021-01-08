//! Support to download from HTTP sources.
//!
//! Specifically this supports the [`HttpSourceConfig`] source.

use std::path::PathBuf;
use std::sync::Arc;

use futures::prelude::*;
use reqwest::header;
use url::Url;

use super::{DownloadError, DownloadStatus, USER_AGENT};
use crate::sources::{FileType, HttpSourceConfig, SourceFileId, SourceLocation};
use crate::types::ObjectId;
use crate::utils::futures as future_utils;

/// Joins the relative path to the given URL.
///
/// As opposed to [`Url::join`], this only supports relative paths. Each segment of the path is
/// percent-encoded. Empty segments are skipped, for example, `foo//bar` is collapsed to `foo/bar`.
///
/// The base URL is treated as directory. If it does not end with a slash, then a slash is
/// automatically appended.
///
/// Returns `Err(())` if the URL is cannot-be-a-base.
fn join_url_encoded(base: &Url, path: &SourceLocation) -> Result<Url, ()> {
    let mut joined = base.clone();
    joined
        .path_segments_mut()?
        .pop_if_empty()
        .extend(path.segments());
    Ok(joined)
}

/// Downloader implementation that supports the [`HttpSourceConfig`] source.
#[derive(Debug)]
pub struct HttpDownloader {
    client: reqwest::Client,
}

impl HttpDownloader {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub async fn download_source(
        &self,
        source: Arc<HttpSourceConfig>,
        download_path: SourceLocation,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        // This can effectively never error since the URL is always validated to be a base URL.
        // Though unfortunately this happens outside of this service, so ideally we'd fix this.
        let download_url = match join_url_encoded(&source.url, &download_path) {
            Ok(x) => x,
            Err(_) => return Ok(DownloadStatus::NotFound),
        };

        log::debug!("Fetching debug file from {}", download_url);
        let response = future_utils::retry(|| {
            let mut builder = self.client.get(download_url.as_str());

            for (key, value) in source.headers.iter() {
                if let Ok(key) = header::HeaderName::from_bytes(key.as_bytes()) {
                    builder = builder.header(key, value.as_str());
                }
            }

            builder.header(header::USER_AGENT, USER_AGENT).send()
        })
        .await;

        match response {
            Ok(response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", download_url);
                    let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

                    super::download_stream(
                        SourceFileId::Http(source, download_path),
                        stream,
                        destination,
                    )
                    .await
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
    ) -> Vec<SourceFileId> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id: &object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| SourceFileId::Http(source.clone(), loc))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sources::SourceConfig;
    use crate::test;

    #[tokio::test]
    async fn test_download_source() {
        test::setup_logging();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path().to_owned();

        let (_srv, source) = test::symbol_server();
        let http_source = match source {
            SourceConfig::Http(source) => source,
            _ => panic!("unexpected source"),
        };
        let loc = SourceLocation::new("hello.txt");

        let downloader = HttpDownloader::new();
        let download_status = downloader
            .download_source(http_source, loc, dest.clone())
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::Completed);

        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n");
    }

    #[tokio::test]
    async fn test_download_source_missing() {
        test::setup_logging();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path().to_owned();

        let (_srv, source) = test::symbol_server();
        let http_source = match source {
            SourceConfig::Http(source) => source,
            _ => panic!("unexpected source"),
        };
        let loc = SourceLocation::new("i-do-not-exist");

        let downloader = HttpDownloader::new();
        let download_status = downloader
            .download_source(http_source, loc, dest)
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::NotFound);
    }

    #[test]
    fn test_join_empty() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("")).unwrap();
        assert_eq!(joined, "https://example.org/base".parse().unwrap());
    }

    #[test]
    fn test_join_space() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo bar")).unwrap();
        assert_eq!(
            joined,
            "https://example.org/base/foo%20bar".parse().unwrap()
        );
    }

    #[test]
    fn test_join_multiple() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo/bar")).unwrap();
        assert_eq!(joined, "https://example.org/base/foo/bar".parse().unwrap());
    }

    #[test]
    fn test_join_trailing_slash() {
        let base = Url::parse("https://example.org/base/").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo")).unwrap();
        assert_eq!(joined, "https://example.org/base/foo".parse().unwrap());
    }

    #[test]
    fn test_join_leading_slash() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("/foo")).unwrap();
        assert_eq!(joined, "https://example.org/base/foo".parse().unwrap());
    }

    #[test]
    fn test_join_multi_slash() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo//bar")).unwrap();
        assert_eq!(joined, "https://example.org/base/foo/bar".parse().unwrap());
    }

    #[test]
    fn test_join_absolute() {
        let base = Url::parse("https://example.org/").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo")).unwrap();
        assert_eq!(joined, "https://example.org/foo".parse().unwrap());
    }
}
