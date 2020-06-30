//! Support to download from sentry sources.
//!
//! Specifically this supports the [`SentrySourceConfig`] source, which allows
//! to fetch files which were directly uploaded to Sentry itself.
//!
//! [`SentrySourceConfig`]: ../../../sources/struct.SentrySourceConfig.html

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix::{Actor, Addr};
use actix_web::client::ClientConnector;
use actix_web::{client, HttpMessage};
use failure::Fail;
use futures01::future::Either;
use futures01::prelude::*;
use parking_lot::Mutex;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::Url;

use super::{DownloadError, DownloadErrorKind, DownloadStatus, USER_AGENT};
use crate::sources::{FileType, SentryFileId, SentrySourceConfig, SourceFileId};
use crate::types::ObjectId;

lazy_static::lazy_static! {
    static ref CLIENT_CONNECTOR: Addr<ClientConnector> = ClientConnector::default().start();
    static ref SENTRY_SEARCH_RESULTS: Mutex<lru::LruCache<SearchQuery, (Instant, Vec<SearchResult>)>> =
        Mutex::new(lru::LruCache::new(100_000));
}

pub fn download_source(
    source: Arc<SentrySourceConfig>,
    file_id: SentryFileId,
    destination: PathBuf,
) -> Box<dyn Future<Item = DownloadStatus, Error = DownloadError>> {
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

    super::download_future_stream(
        SourceFileId::Sentry(source, file_id),
        Box::new(response),
        destination,
    )
}

#[derive(Debug, Fail, Clone, Copy)]
pub enum SentryErrorKind {
    #[fail(display = "failed parsing JSON response from Sentry")]
    Parsing,

    #[fail(display = "bad status code from Sentry")]
    BadStatusCode,

    #[fail(display = "failed sending request to Sentry")]
    SendRequest,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct SearchResult {
    id: String,
    // TODO: Add more fields
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SearchQuery {
    index_url: Url,
    token: String,
}

symbolic::common::derive_failure!(
    SentryError,
    SentryErrorKind,
    doc = "Errors happening while fetching data from Sentry"
);

fn perform_search(
    query: SearchQuery,
) -> impl Future<Item = Vec<SearchResult>, Error = DownloadError> {
    if let Some((created, entries)) = SENTRY_SEARCH_RESULTS.lock().get(&query) {
        if created.elapsed() < Duration::from_secs(3600) {
            return Either::A(Ok(entries.clone()).into_future());
        }
    }

    let index_url = query.index_url.clone();
    let token = query.token.clone();

    log::debug!("Fetching list of Sentry debug files from {}", index_url);
    let index_request = move || {
        client::get(&index_url)
            .with_connector((*CLIENT_CONNECTOR).clone())
            .header("Accept-Encoding", "identity")
            .header("User-Agent", USER_AGENT)
            .header("Authorization", format!("Bearer {}", token.clone()))
            .finish()
            .unwrap()
            .send()
            .map_err(|e| e.context(SentryErrorKind::SendRequest).into())
            .and_then(move |response| {
                if response.status().is_success() {
                    log::trace!("Success fetching index from Sentry");
                    Either::A(
                        response
                            .json::<Vec<SearchResult>>()
                            .map_err(|e| e.context(SentryErrorKind::Parsing).into()),
                    )
                } else {
                    log::warn!("Sentry returned status code {}", response.status());
                    Either::B(Err(SentryError::from(SentryErrorKind::BadStatusCode)).into_future())
                }
            })
    };

    let index_request = Retry::spawn(
        ExponentialBackoff::from_millis(10).map(jitter).take(3),
        index_request,
    );

    let index_request = index_request.map(move |entries| {
        SENTRY_SEARCH_RESULTS
            .lock()
            .put(query, (Instant::now(), entries.clone()));
        entries
    });

    Either::B(
        future_metrics!("downloads.sentry.index", None, index_request).map_err(|e| match e {
            tokio_retry::Error::OperationError(e) => e.context(DownloadErrorKind::Sentry).into(),
            e => panic!("{}", e),
        }),
    )
}

pub fn list_files(
    source: Arc<SentrySourceConfig>,
    _filetypes: &'static [FileType],
    object_id: ObjectId,
) -> impl Future<Item = Vec<SourceFileId>, Error = DownloadError> {
    let index_url = {
        let mut url = source.url.clone();
        if let Some(ref debug_id) = object_id.debug_id {
            url.query_pairs_mut()
                .append_pair("debug_id", &debug_id.to_string());
        }

        if let Some(ref code_id) = object_id.code_id {
            url.query_pairs_mut()
                .append_pair("code_id", &code_id.to_string());
        }

        url
    };

    let query = SearchQuery {
        index_url,
        token: source.token.clone(),
    };

    let entries = perform_search(query).map(clone!(source, |entries| {
        entries
            .into_iter()
            .map(move |api_response| {
                SourceFileId::Sentry(source.clone(), SentryFileId::new(api_response.id))
            })
            .collect()
    }));

    Box::new(entries)
}
