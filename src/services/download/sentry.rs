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
use actix_web::client::{ClientConnector, ClientResponse, SendRequestError};
use actix_web::error::JsonPayloadError;
use actix_web::http::StatusCode;
use actix_web::{client, HttpMessage};
use failure::Fail;
use futures::compat::{Future01CompatExt, Stream01CompatExt};
use futures::prelude::*;
use parking_lot::Mutex;
use thiserror::Error;
use url::Url;

use super::{DownloadError, DownloadStatus, USER_AGENT};
use crate::config::Config;
use crate::sources::{FileType, SentryFileId, SentrySourceConfig, SourceFileId};
use crate::types::ObjectId;
use crate::utils::futures as future_utils;

lazy_static::lazy_static! {
    static ref CLIENT_CONNECTOR: Addr<ClientConnector> = ClientConnector::default().start();
    static ref SENTRY_SEARCH_RESULTS: Mutex<lru::LruCache<SearchQuery, (Instant, Vec<SearchResult>)>> =
        Mutex::new(lru::LruCache::new(100_000));
}

async fn start_request(
    source: &SentrySourceConfig,
    url: &Url,
) -> Result<ClientResponse, SendRequestError> {
    // The timeout is for the entire HTTP download *including* the response stream itself,
    // in contrast to what the Actix-Web docs say. We have tested this manually.
    //
    // The intent is to disable the timeout entirely, but there is no API for that.
    client::get(url)
        .with_connector((*CLIENT_CONNECTOR).clone())
        .header("User-Agent", USER_AGENT)
        .header("Authorization", format!("Bearer {}", &source.token))
        .timeout(Duration::from_secs(9999))
        .finish()
        .unwrap()
        .send()
        .compat()
        .await
}

pub async fn download_source(
    source: Arc<SentrySourceConfig>,
    file_id: SentryFileId,
    destination: PathBuf,
) -> Result<DownloadStatus, DownloadError> {
    let download_url = source.download_url(&file_id);
    log::debug!("Fetching debug file from {}", download_url);
    let response = future_utils::retry(|| start_request(&source, &download_url)).await;

    match response {
        Ok(response) => {
            if response.status().is_success() {
                log::trace!("Success hitting {}", download_url);
                let stream = response
                    .payload()
                    .compat()
                    .map(|i| i.map_err(DownloadError::stream));
                super::download_stream(SourceFileId::Sentry(source, file_id), stream, destination)
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

/// Errors happening while fetching data from Sentry.
#[derive(Debug, Error)]
pub enum SentryError {
    #[error("Failed parsing JSON response from Sentry")]
    Parsing(#[from] failure::Compat<JsonPayloadError>),

    #[error("Failed sending request to Sentry")]
    SendRequest(#[from] failure::Compat<SendRequestError>),

    #[error("Bad status code from Sentry: {0}")]
    BadStatusCode(StatusCode),
}

/// Make a request to sentry, parse the result as a JSON SearchResult list.
async fn fetch_sentry_json(query: &SearchQuery) -> Result<Vec<SearchResult>, SentryError> {
    let response = client::get(&query.index_url)
        .with_connector((*CLIENT_CONNECTOR).clone())
        .header("Accept-Encoding", "identity")
        .header("User-Agent", USER_AGENT)
        .header("Authorization", format!("Bearer {}", &query.token))
        .finish()
        .unwrap()
        .send()
        .compat()
        .await
        .map_err(Fail::compat)?;
    if response.status().is_success() {
        log::trace!("Success fetching index from Sentry");
        response
            .json()
            .compat()
            .await
            .map_err(|e| Fail::compat(e).into())
    } else {
        log::warn!("Sentry returned status code {}", response.status());
        Err(SentryError::BadStatusCode(response.status()))
    }
}

/// Return the search results.
///
/// If there are cached search results this skips the actual search.
async fn cached_sentry_search(
    query: SearchQuery,
    config: Arc<Config>,
) -> Result<Vec<SearchResult>, DownloadError> {
    // The Sentry cache index should expire as soon as we attempt to retry negative caches.
    let cache_duration = if config.cache_dir.is_some() {
        config
            .caches
            .downloaded
            .retry_misses_after
            .unwrap_or_else(|| Duration::from_secs(0))
    } else {
        Duration::from_secs(0)
    };

    if let Some((created, entries)) = SENTRY_SEARCH_RESULTS.lock().get(&query) {
        if created.elapsed() < cache_duration {
            return Ok(entries.clone());
        }
    }

    log::debug!(
        "Fetching list of Sentry debug files from {}",
        &query.index_url
    );
    let entries = future_utils::retry(|| fetch_sentry_json(&query)).await?;

    if cache_duration > Duration::from_secs(0) {
        SENTRY_SEARCH_RESULTS
            .lock()
            .put(query, (Instant::now(), entries.clone()));
    }

    Ok(entries)
}

pub async fn list_files(
    source: Arc<SentrySourceConfig>,
    _filetypes: &'static [FileType],
    object_id: ObjectId,
    config: Arc<Config>,
) -> Result<Vec<SourceFileId>, DownloadError> {
    // There needs to be either a debug_id or a code_id filter in the query. Otherwise, this would
    // return a list of all debug files in the project.
    if object_id.debug_id.is_none() && object_id.code_id.is_none() {
        return Ok(Vec::new());
    }

    let mut index_url = source.url.clone();
    if let Some(ref debug_id) = object_id.debug_id {
        index_url
            .query_pairs_mut()
            .append_pair("debug_id", &debug_id.to_string());
    }

    if let Some(ref code_id) = object_id.code_id {
        index_url
            .query_pairs_mut()
            .append_pair("code_id", &code_id.to_string());
    }

    let query = SearchQuery {
        index_url,
        token: source.token.clone(),
    };

    let search = cached_sentry_search(query, config);
    let entries = future_utils::time_task("downloads.sentry.index", search).await?;
    let file_ids = entries
        .into_iter()
        .map(|search_result| {
            SourceFileId::Sentry(source.clone(), SentryFileId::new(search_result.id))
        })
        .collect();

    Ok(file_ids)
}
