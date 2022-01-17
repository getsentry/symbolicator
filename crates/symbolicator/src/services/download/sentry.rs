//! Support to download from sentry sources.
//!
//! Specifically this supports the [`SentrySourceConfig`] source, which allows
//! to fetch files which were directly uploaded to Sentry itself.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::TryStreamExt;
use parking_lot::Mutex;
use reqwest::{header, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use super::{
    content_length_timeout, DownloadError, DownloadStatus, FileType, RemoteDif, RemoteDifUri,
    USER_AGENT,
};
use crate::config::Config;
use crate::sources::SentrySourceConfig;
use crate::types::ObjectId;
use crate::utils::futures::{self as future_utils};

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
    connect_timeout: Duration,
    streaming_timeout: Duration,
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
    pub fn new(
        client: reqwest::Client,
        connect_timeout: Duration,
        streaming_timeout: Duration,
    ) -> Self {
        Self {
            client,
            index_cache: Mutex::new(SentryIndexCache::new(100_000)),
            connect_timeout,
            streaming_timeout,
        }
    }

    /// Make a request to sentry, parse the result as a JSON SearchResult list.
    async fn fetch_sentry_json(
        &self,
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, SentryError> {
        let mut request = self
            .client
            .get(query.index_url.clone())
            .bearer_auth(&query.token)
            .header("Accept-Encoding", "identity")
            .header("User-Agent", USER_AGENT);
        if let Some(span) = sentry::configure_scope(|scope| scope.get_span()) {
            for (k, v) in span.iter_headers() {
                request = request.header(k, v);
            }
        }

        let response = request.send().await?;

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
        config: &Config,
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
        config: &Config,
    ) -> Result<Vec<RemoteDif>, DownloadError> {
        // TODO(flub): These queries do not handle pagination.  But sentry only starts to
        // paginate at 20 results so we get away with this for now.

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

        // See <sentry-repo>/src/sentry/constants.py KNOWN_DIF_FORMATS for these query strings.
        index_url.query_pairs_mut().extend_pairs(
            file_types
                .iter()
                .map(|file_type| match file_type {
                    FileType::UuidMap => "uuidmap",
                    FileType::BcSymbolMap => "bcsymbolmap",
                    FileType::Pe => "pe",
                    FileType::Pdb => "pdb",
                    FileType::MachDebug | FileType::MachCode => "macho",
                    FileType::ElfDebug | FileType::ElfCode => "elf",
                    FileType::WasmDebug | FileType::WasmCode => "wasm",
                    FileType::Breakpad => "breakpad",
                    FileType::SourceBundle => "sourcebundle",
                })
                .map(|val| ("file_formats", val)),
        );

        if let Some(ref code_id) = object_id.code_id {
            index_url
                .query_pairs_mut()
                .append_pair("code_id", code_id.as_str());
        }

        let query = SearchQuery {
            index_url,
            token: source.token.clone(),
        };

        let search = self.cached_sentry_search(query, config).await?;
        let file_ids = search
            .into_iter()
            .map(|search_result| SentryRemoteDif::new(source.clone(), search_result.id).into())
            .collect();

        Ok(file_ids)
    }

    /// Downloads a source hosted on Sentry.
    ///
    /// # Directly thrown errors
    /// - [`DownloadError::Reqwest`]
    /// - [`DownloadError::Rejected`]
    /// - [`DownloadError::Canceled`]
    pub async fn download_source(
        &self,
        file_source: SentryRemoteDif,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        let request = self
            .client
            .get(file_source.url())
            .header("User-Agent", USER_AGENT)
            .bearer_auth(&file_source.source.token)
            .send();

        let download_url = file_source.url();
        let source = RemoteDif::from(file_source);
        let request = tokio::time::timeout(self.connect_timeout, request);
        let request = super::measure_download_time(source.source_metric_key(), request);

        match request.await {
            Ok(Ok(response)) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", download_url);

                    let content_length = response
                        .headers()
                        .get(header::CONTENT_LENGTH)
                        .and_then(|hv| hv.to_str().ok())
                        .and_then(|s| s.parse::<u32>().ok());

                    let timeout =
                        content_length.map(|cl| content_length_timeout(cl, self.streaming_timeout));
                    let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

                    super::download_stream(&source, stream, destination, timeout).await
                } else if matches!(
                    response.status(),
                    StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
                ) {
                    log::debug!("Insufficient permissions to download from {}", download_url);
                    Err(DownloadError::Permissions)
                } else if response.status().is_client_error() {
                    log::debug!(
                        "Unexpected client error status code from {}: {}",
                        download_url,
                        response.status()
                    );
                    Ok(DownloadStatus::NotFound)
                } else {
                    log::debug!(
                        "Unexpected status code from {}: {}",
                        download_url,
                        response.status()
                    );
                    Err(DownloadError::Rejected(response.status()))
                }
            }
            Ok(Err(e)) => {
                log::debug!("Skipping response from {}: {}", download_url, e);
                Err(DownloadError::Reqwest(e)) // must be wrong type
            }
            // Timed out
            Err(_) => Err(DownloadError::Canceled),
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
