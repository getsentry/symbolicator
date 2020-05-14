use std::sync::Arc;
use std::time::Duration;

use actix_web::http::header;
use actix_web::{client, HttpMessage};
use failure::Fail;
use futures01::{future, Future, Stream};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::Url;

use crate::actors::objects::common::prepare_download_paths;
use crate::actors::objects::{
    DownloadPath, DownloadStream, FileId, ObjectError, ObjectErrorKind, USER_AGENT,
};
use crate::types::{FileType, HttpSourceConfig, ObjectId};
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
fn join_url_encoded(base: &Url, path: &str) -> Result<Url, ()> {
    let mut joined = base.clone();
    joined
        .path_segments_mut()?
        .pop_if_empty()
        .extend(path.split('/').filter(|s| !s.is_empty()));
    Ok(joined)
}

pub(super) fn prepare_downloads(
    source: &Arc<HttpSourceConfig>,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> Box<dyn Future<Item = Vec<FileId>, Error = ObjectError>> {
    let ids = prepare_download_paths(
        object_id,
        filetypes,
        &source.files.filters,
        source.files.layout,
    )
    .map(|download_path| FileId::Http(source.clone(), download_path))
    .collect();

    Box::new(future::ok(ids))
}

pub(super) fn download_from_source(
    source: Arc<HttpSourceConfig>,
    download_path: &DownloadPath,
) -> Box<dyn Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    // This can effectively never error since the URL is always validated to be a base URL.
    let download_url = match join_url_encoded(&source.url, &download_path.0) {
        Ok(x) => x,
        Err(_) => return Box::new(future::ok(None)),
    };

    log::debug!("Fetching debug file from {}", download_url);
    let response = clone!(download_url, source, || {
        http::follow_redirects(
            download_url.clone(),
            MAX_HTTP_REDIRECTS,
            clone!(source, |url| {
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
            }),
        )
    });

    let response = Retry::spawn(
        ExponentialBackoff::from_millis(10).map(jitter).take(3),
        response,
    );

    let response = response.map_err(|e| match e {
        tokio_retry::Error::OperationError(e) => e,
        e => panic!("{}", e),
    });

    let response = response.then(move |result| match result {
        Ok(response) => {
            if response.status().is_success() {
                log::trace!("Success hitting {}", download_url);
                Ok(Some(DownloadStream::FutureStream(Box::new(
                    response
                        .payload()
                        .map_err(|e| e.context(ObjectErrorKind::Io).into()),
                )
                    as Box<dyn Stream<Item = _, Error = _>>)))
            } else {
                log::trace!(
                    "Unexpected status code from {}: {}",
                    download_url,
                    response.status()
                );
                Ok(None)
            }
        }
        Err(e) => {
            log::trace!("Skipping response from {}: {}", download_url, e);
            Ok(None)
        }
    });

    Box::new(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_empty() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, "").unwrap();
        assert_eq!(joined, "https://example.org/base".parse().unwrap());
    }

    #[test]
    fn test_join_space() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, "foo bar").unwrap();
        assert_eq!(
            joined,
            "https://example.org/base/foo%20bar".parse().unwrap()
        );
    }

    #[test]
    fn test_join_multiple() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, "foo/bar").unwrap();
        assert_eq!(joined, "https://example.org/base/foo/bar".parse().unwrap());
    }

    #[test]
    fn test_join_trailing_slash() {
        let base = Url::parse("https://example.org/base/").unwrap();
        let joined = join_url_encoded(&base, "foo").unwrap();
        assert_eq!(joined, "https://example.org/base/foo".parse().unwrap());
    }

    #[test]
    fn test_join_leading_slash() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, "/foo").unwrap();
        assert_eq!(joined, "https://example.org/base/foo".parse().unwrap());
    }

    #[test]
    fn test_join_multi_slash() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, "foo//bar").unwrap();
        assert_eq!(joined, "https://example.org/base/foo/bar".parse().unwrap());
    }

    #[test]
    fn test_join_absolute() {
        let base = Url::parse("https://example.org/").unwrap();
        let joined = join_url_encoded(&base, "foo").unwrap();
        assert_eq!(joined, "https://example.org/foo".parse().unwrap());
    }
}
