//! Support to download from sentry sources.
//!
//! Specifically this supports the [`SentrySourceConfig`] source, which allows
//! to fetch files which were directly uploaded to Sentry itself.

use std::fmt;
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
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use super::{DownloadError, DownloadStatus, ObjectFileSource, USER_AGENT};
use crate::config::Config;
use crate::sources::{FileType, SentrySourceConfig};
use crate::types::{ObjectFileSourceURI, ObjectId};
use crate::utils::futures as future_utils;

/// The Sentry-specific [`ObjectFileSource`].
#[derive(Debug, Clone)]
pub struct SentryObjectFileSource {
    pub source: Arc<SentrySourceConfig>,
    pub file_id: SentryFileId,
}

impl From<SentryObjectFileSource> for ObjectFileSource {
    fn from(source: SentryObjectFileSource) -> Self {
        Self::Sentry(source)
    }
}

impl SentryObjectFileSource {
    pub fn new(source: Arc<SentrySourceConfig>, file_id: SentryFileId) -> Self {
        Self { source, file_id }
    }

    pub fn uri(&self) -> ObjectFileSourceURI {
        self.url().as_str().into()
    }

    /// Returns the URL from which to download this object file.
    pub fn url(&self) -> Url {
        let mut url = self.source.url.clone();
        url.query_pairs_mut().append_pair("id", &self.file_id.0);
        url
    }
}

/// An identifier for a file retrievable from a [`SentrySourceConfig`].
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize)]
pub struct SentryFileId(String);

impl fmt::Display for SentryFileId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
struct SearchResult {
    id: SentryFileId,
    // TODO: Add more fields
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SearchQuery {
    index_url: Url,
    token: String,
}

/// An LRU cache sentry DIF index responses.
type SentryIndexCache = lru::LruCache<SearchQuery, (Instant, Vec<SearchResult>)>;

/// Errors happening while fetching data from Sentry.
#[derive(Debug, Error)]
pub enum SentryError {
    #[error("failed parsing JSON response from Sentry")]
    Parsing(#[from] failure::Compat<JsonPayloadError>),

    #[error("failed sending request to Sentry")]
    SendRequest(#[from] failure::Compat<SendRequestError>),

    #[error("bad status code from Sentry: {0}")]
    BadStatusCode(StatusCode),
}

pub struct SentryDownloader {
    connector: Addr<ClientConnector>,
    index_cache: Mutex<SentryIndexCache>,
}

impl fmt::Debug for SentryDownloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(std::any::type_name::<Self>())
            .field("connector", &format_args!("Addr(ClientConnector)"))
            .field("index_cache", &self.index_cache)
            .finish()
    }
}

impl SentryDownloader {
    pub fn new() -> Self {
        Self {
            connector: ClientConnector::default().start(),
            index_cache: Mutex::new(SentryIndexCache::new(100_000)),
        }
    }

    async fn start_request(
        &self,
        source: &SentrySourceConfig,
        url: &Url,
    ) -> Result<ClientResponse, SendRequestError> {
        // The timeout is for the entire HTTP download *including* the response stream itself,
        // in contrast to what the Actix-Web docs say. We have tested this manually.
        //
        // The intent is to disable the timeout entirely, but there is no API for that.
        client::get(url)
            .with_connector(self.connector.clone())
            .header("User-Agent", USER_AGENT)
            .header("Authorization", format!("Bearer {}", &source.token))
            .timeout(Duration::from_secs(9999))
            .finish()
            .unwrap()
            .send()
            .compat()
            .await
    }

    /// Make a request to sentry, parse the result as a JSON SearchResult list.
    async fn fetch_sentry_json(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, SentryError> {
        let response = client::get(&query.index_url)
            .with_connector(self.connector.clone())
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
        &self,
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

        if let Some((created, entries)) = self.index_cache.lock().get(&query) {
            if created.elapsed() < cache_duration {
                return Ok(entries.clone());
            }
        }

        log::debug!(
            "Fetching list of Sentry debug files from {}",
            &query.index_url
        );
        let entries = future_utils::retry(|| self.fetch_sentry_json(&query)).await?;

        if cache_duration > Duration::from_secs(0) {
            self.index_cache
                .lock()
                .put(query, (Instant::now(), entries.clone()));
        }

        Ok(entries)
    }

    pub async fn list_files(
        &self,
        source: Arc<SentrySourceConfig>,
        _filetypes: &'static [FileType],
        object_id: ObjectId,
        config: Arc<Config>,
    ) -> Result<Vec<ObjectFileSource>, DownloadError> {
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

        let search = self.cached_sentry_search(query, config);
        let entries = future_utils::time_task("downloads.sentry.index", search).await?;
        let file_ids = entries
            .into_iter()
            .map(|search_result| {
                SentryObjectFileSource::new(source.clone(), search_result.id).into()
            })
            .collect();

        Ok(file_ids)
    }

    pub async fn download_source(
        &self,
        file_source: SentryObjectFileSource,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let download_url = file_source.url();
        log::debug!("Fetching debug file from {}", download_url);
        let response =
            future_utils::retry(|| self.start_request(&file_source.source, &download_url)).await;

        match response {
            Ok(response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", download_url);
                    let stream = response
                        .payload()
                        .compat()
                        .map(|i| i.map_err(DownloadError::stream));
                    super::download_stream(ObjectFileSource::from(file_source), stream, destination)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sources::SourceId;

    #[test]
    fn test_sentry_source_download_url() {
        let source = SentrySourceConfig {
            id: SourceId::new("test"),
            url: Url::parse("https://example.net/endpoint/").unwrap(),
            token: "token".into(),
        };
        let file_source =
            SentryObjectFileSource::new(Arc::new(source), SentryFileId("abc123".into()));
        let url = file_source.url();
        assert_eq!(url.as_str(), "https://example.net/endpoint/?id=abc123");
    }
}
