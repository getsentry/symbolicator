//! Support to download from sentry sources.
//!
//! Specifically this supports the [`SentrySourceConfig`] source, which allows
//! to fetch files which were directly uploaded to Sentry itself.
//!
//! [`SentrySourceConfig`]: ../../../sources/struct.SentrySourceConfig.html

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, Addr};
use actix_web::client::ClientConnector;
use actix_web::{client, HttpMessage};
use failure::{Fail, ResultExt};
use futures01::future;
use futures01::prelude::*;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;

use super::clients::USER_AGENT;
use super::types::{DownloadError, DownloadErrorKind, DownloadStatus, DownloadStream};
use crate::sources::{SentryFileId, SentrySourceConfig};

lazy_static::lazy_static! {
    // static ref SENTRY_SEARCH_RESULTS: Mutex<lru::LruCache<SearchQuery, (Instant, Vec<SearchResult>)>> =
    //     Mutex::new(lru::LruCache::new(100_000));
    static ref CLIENT_CONNECTOR: Addr<ClientConnector> = ClientConnector::default().start();
}

/// Download from a Sentry source.
///
/// See [`Downloader::download`] for the semantics of the file being written at `dest`.
///
/// [`Downloader::download`]: ../struct.Downloader.html#method.download
pub fn download_source(
    source: Arc<SentrySourceConfig>,
    location: SentryFileId,
    dest: PathBuf,
) -> Box<dyn Future<Item = DownloadStatus, Error = DownloadError>> {
    // All file I/O in this function is blocking!
    let source2 = source.clone();
    let ret =
        download_from_source(source, &location).and_then(move |maybe_stream| match maybe_stream {
            Some(stream) => {
                log::trace!(
                    "Downloading file for Sentry source {id} location {loc}",
                    id = source2.id,
                    loc = location
                );
                let file =
                    tryf!(std::fs::File::create(&dest).context(DownloadErrorKind::BadDestination));
                let fut = stream
                    .fold(file, |mut file, chunk| {
                        file.write_all(chunk.as_ref())
                            .context(DownloadErrorKind::Write)
                            .map_err(DownloadError::from)
                            .map(|_| file)
                    })
                    .and_then(|_| Ok(DownloadStatus::Completed));
                Box::new(fut) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
            }
            None => {
                let fut = future::ok(DownloadStatus::NotFound);
                Box::new(fut) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
            }
        });
    Box::new(ret) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
}

fn download_from_source(
    source: Arc<SentrySourceConfig>,
    file_id: &SentryFileId,
) -> Box<dyn Future<Item = Option<DownloadStream>, Error = DownloadError>> {
    let download_url = {
        let mut url = source.url.clone();
        url.query_pairs_mut().append_pair("id", &file_id.0);
        url
    };

    log::debug!("Fetching debug file from {}", download_url);
    let token = &source.token;
    let response = clone!(token, download_url, || {
        client::get(&download_url)
            .with_connector((*CLIENT_CONNECTOR).clone())
            .header("User-Agent", USER_AGENT)
            .header("Authorization", format!("Bearer {}", token))
            // This timeout is for the entire HTTP download *including* the response stream
            // itself, in contrast to what the Actix-Web docs say. We have tested this
            // manually.
            //
            // The intent is to disable the timeout entirely, but there is no API for that.
            .timeout(Duration::from_secs(9999))
            .finish()
            .unwrap()
            .send()
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
                Ok(Some(Box::new(
                    response
                        .payload()
                        .map_err(|e| e.context(DownloadErrorKind::Io).into()),
                )
                    as Box<dyn Stream<Item = _, Error = _>>))
            } else {
                log::debug!(
                    "Unexpected status code from {}: {}",
                    download_url,
                    response.status()
                );
                Ok(None)
            }
        }
        Err(e) => {
            log::warn!("Skipping response from {}: {}", download_url, e);
            Ok(None)
        }
    });

    Box::new(response)
}
