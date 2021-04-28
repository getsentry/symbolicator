//! Support to download from sentry sources.
//!
//! Specifically this supports the [`SentrySourceConfig`] source, which allows
//! to fetch files which were directly uploaded to Sentry itself.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::prelude::*;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use super::{DownloadError, DownloadStatus, FileType, RemoteDif, RemoteDifUri, USER_AGENT};
use crate::config::Config;
use crate::sources::SentrySourceConfig;
use crate::types::ObjectId;
use crate::utils::futures::{self as future_utils, m, measure};

/// The Sentry-specific [`RemoteDif`].
#[derive(Debug, Clone)]
pub struct SentryRemoteDif {
    pub source: Arc<SentrySourceConfig>,
    pub file_id: SentryFileId,
}

impl From<SentryRemoteDif> for RemoteDif {
    fn from(source: SentryRemoteDif) -> Self {
        Self::Sentry(source)
    }
}

impl SentryRemoteDif {
    pub fn new(source: Arc<SentrySourceConfig>, file_id: SentryFileId) -> Self {
        Self { source, file_id }
    }

    pub fn uri(&self) -> RemoteDifUri {
        format!("sentry://project_debug_file/{}", self.file_id).into()
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
    #[error("failed sending request to Sentry")]
    Reqwest(#[from] reqwest::Error),

    #[error("bad status code from Sentry: {0}")]
    BadStatusCode(StatusCode),
}

pub struct SentryDownloader {
    client: reqwest::Client,
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
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            index_cache: Mutex::new(SentryIndexCache::new(100_000)),
        }
    }

    /// Make a request to sentry, parse the result as a JSON SearchResult list.
    async fn fetch_sentry_json(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, SentryError> {
        let response = self
            .client
            .get(query.index_url.clone())
            .header("Accept-Encoding", "identity")
            .header("User-Agent", USER_AGENT)
            .bearer_auth(&query.token)
            .send()
            .await?;

        if response.status().is_success() {
            log::trace!("Success fetching index from Sentry");
            Ok(response.json().await?)
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
        object_id: ObjectId,
        file_types: &[FileType],
        config: Arc<Config>,
    ) -> Result<Vec<RemoteDif>, DownloadError> {
        // TODO(flub): LOL, we don't even handle pagination.  But sentry only starts to
        // paginate at 20 results so i guess we get away with this for now.

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
        for file_type in file_types {
            match file_type {
                FileType::PList => {
                    index_url
                        .query_pairs_mut()
                        .append_pair("file_formats", "plist");
                }
                FileType::BcSymbolMap => {
                    index_url
                        .query_pairs_mut()
                        .append_pair("file_formats", "bcsymbolmap");
                }
                // We do not currently attempt to filter objects
                // TODO(flub): check object actor handles getting a plist correctly.
                _ => (),
            }
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
        let entries = measure("downloads.sentry.index", m::result, search).await?;
        let file_ids = entries
            .into_iter()
            .map(|search_result| SentryRemoteDif::new(source.clone(), search_result.id).into())
            .collect();

        Ok(file_ids)
    }

    pub async fn download_source(
        &self,
        file_source: SentryRemoteDif,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let retries = future_utils::retry(|| {
            self.download_source_once(file_source.clone(), destination.clone())
        });
        match retries.await {
            Ok(status) => {
                log::debug!(
                    "Fetched debug file from {}: {:?}",
                    file_source.url(),
                    status
                );
                Ok(status)
            }
            Err(err) => {
                log::debug!(
                    "Failed to fetch debug file from {}: {}",
                    file_source.url(),
                    err
                );
                Err(err)
            }
        }
    }

    async fn download_source_once(
        &self,
        source: SentryRemoteDif,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let request = self
            .client
            .get(source.url())
            .header("User-Agent", USER_AGENT)
            .bearer_auth(&source.source.token)
            .send();
        match request.await {
            Ok(response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", source.url());
                    let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

                    super::download_stream(source, stream, destination).await
                } else {
                    log::trace!(
                        "Unexpected status code from {}: {}",
                        source.url(),
                        response.status()
                    );
                    Ok(DownloadStatus::NotFound)
                }
            }
            Err(e) => {
                log::trace!("Skipping response from {}: {}", source.url(), e);
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
    fn test_download_url() {
        let source = SentrySourceConfig {
            id: SourceId::new("test"),
            url: Url::parse("https://example.net/endpoint/").unwrap(),
            token: "token".into(),
        };
        let file_source = SentryRemoteDif::new(Arc::new(source), SentryFileId("abc123".into()));
        let url = file_source.url();
        assert_eq!(url.as_str(), "https://example.net/endpoint/?id=abc123");
    }

    #[test]
    fn test_uri() {
        let source = SentrySourceConfig {
            id: SourceId::new("test"),
            url: Url::parse("https://example.net/endpoint/").unwrap(),
            token: "token".into(),
        };
        let file_source = SentryRemoteDif::new(Arc::new(source), SentryFileId("abc123".into()));
        let uri = file_source.uri();
        assert_eq!(uri, RemoteDifUri::new("sentry://project_debug_file/abc123"));
    }
}
