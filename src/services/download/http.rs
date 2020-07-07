//! Support to download from HTTP sources.
//!
//! Specifically this supports the [`HttpSourceConfig`] source.
//!
//! [`HttpSourceConfig`]: ../../../sources/struct.HttpSourceConfig.html

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix_web::http::header;
use actix_web::{client, HttpMessage};
use failure::Fail;
use futures::compat::{Future01CompatExt, Stream01CompatExt};
use futures::prelude::*;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::Url;

use super::{DownloadError, DownloadErrorKind, DownloadStatus, USER_AGENT};
use crate::sources::{FileType, HttpSourceConfig, SourceFileId, SourceLocation};
use crate::types::ObjectId;
use crate::utils::http;

/// The maximum number of redirects permitted by a remote symbol server.
const MAX_HTTP_REDIRECTS: usize = 10;

/// Joins the relative path to the given URL.
///
/// As opposed to `Url::join`, this only supports relative paths. Each segment of the path is
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

/// Start a request to the web server.
///
/// This initiates the request, when the future resolves the headers will have been
/// retrieved but not the entire payload.
async fn start_request(
    source: Arc<HttpSourceConfig>,
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
    let response = Retry::spawn(
        ExponentialBackoff::from_millis(10).map(jitter).take(3),
        || {
            start_request(source.clone(), download_url.clone())
                .boxed_local()
                .compat()
        },
    )
    .compat()
    .await
    .map_err(|e| match e {
        tokio_retry::Error::OperationError(e) => e,
        e => panic!("{}", e),
    });

    match response {
        Ok(response) => {
            if response.status().is_success() {
                log::trace!("Success hitting {}", download_url);
                let stream = response
                    .payload()
                    .compat()
                    .map(|i| i.map_err(|e| e.context(DownloadErrorKind::Io).into()));
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sources::SourceConfig;
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

        let ret = test::block_fn(|| download_source(http_source, loc, dest.clone()));
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

        let ret = test::block_fn(|| download_source(http_source, loc, dest.clone()));
        assert_eq!(ret.unwrap(), DownloadStatus::NotFound);
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
