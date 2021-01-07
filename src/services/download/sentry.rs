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
use thiserror::Error;
use url::Url;

use super::{DownloadError, DownloadStatus, USER_AGENT};
use crate::config::Config;
use crate::sources::{FileType, SentryFileId, SentrySourceConfig, SourceFileId};
use crate::types::ObjectId;
use crate::utils::futures::{self as future_utils, m, measure};

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

/// An LRU cache sentry DIF index responses.
type SentryIndexCache = lru::LruCache<SearchQuery, (Instant, Vec<SearchResult>)>;

/// Errors happening while fetching data from Sentry.
#[derive(Debug, Error)]
pub enum SentryError {
    // TODO(ja): Find better errors here
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
    pub fn new() -> Self {
        Self {
            // TODO(ja): Bypass IP restriction
            client: reqwest::Client::new(),
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
            .get(query.index_url.as_str())
            .header("Accept-Encoding", "identity")
            .header("User-Agent", USER_AGENT)
            .header("Authorization", format!("Bearer {}", &query.token))
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

        let search = self.cached_sentry_search(query, config);
        let entries = measure("downloads.sentry.index", m::result, search).await?;
        let file_ids = entries
            .into_iter()
            .map(|search_result| {
                SourceFileId::Sentry(source.clone(), SentryFileId::new(search_result.id))
            })
            .collect();

        Ok(file_ids)
    }

    pub async fn download_source(
        &self,
        source: Arc<SentrySourceConfig>,
        file_id: SentryFileId,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let download_url = source.download_url(&file_id);
        log::debug!("Fetching debug file from {}", download_url);
        let result = future_utils::retry(|| {
            self.client
                .get(download_url.as_str())
                .header("User-Agent", USER_AGENT)
                .header("Authorization", format!("Bearer {}", &source.token))
                .send()
        })
        .await;

        match result {
            Ok(response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", download_url);
                    let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

                    super::download_stream(
                        SourceFileId::Sentry(source, file_id),
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
}
