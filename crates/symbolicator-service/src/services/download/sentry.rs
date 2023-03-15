//! Support to download from sentry sources.
//!
//! This allows to fetch files which were directly uploaded to Sentry itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sentry::SentryFutureExt;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use symbolicator_sources::{
    ObjectId, RemoteFile, SentryFileId, SentryFileType, SentryRemoteFile, SentrySourceConfig,
};

use super::{FileType, USER_AGENT};
use crate::caching::{CacheEntry, CacheError};
use crate::config::Config;
use crate::utils::futures::CancelOnDrop;

#[derive(Clone, Debug, Deserialize)]
struct JustIdResult {
    pub id: SentryFileId,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ArtifactResult {
    pub id: SentryFileId,
    pub name: String,
    pub sha1: String,
    pub dist: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// We would want to cache differently-shaped results in our lookup cache
#[derive(Clone, Debug)]
enum SearchResult {
    JustId(Vec<JustIdResult>),
    Artifact(Vec<ArtifactResult>),
}

trait IntoSearchResult: Sized {
    fn into_search_result(vec: Vec<Self>) -> SearchResult;
}

impl IntoSearchResult for JustIdResult {
    fn into_search_result(vec: Vec<Self>) -> SearchResult {
        SearchResult::JustId(vec)
    }
}
impl IntoSearchResult for ArtifactResult {
    fn into_search_result(vec: Vec<Self>) -> SearchResult {
        SearchResult::Artifact(vec)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SearchQuery {
    query_url: Url,
    token: String,
}

/// An LRU Cache for Sentry API Lookups.
type SentryLookupCache = moka::future::Cache<SearchQuery, CacheEntry<SearchResult>>;

pub struct SentryDownloader {
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
    lookup_cache: SentryLookupCache,
    connect_timeout: Duration,
    streaming_timeout: Duration,
}

impl fmt::Debug for SentryDownloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(std::any::type_name::<Self>())
            .field("connector", &format_args!("Addr(ClientConnector)"))
            .field("index_cache", &self.lookup_cache)
            .finish()
    }
}

impl SentryDownloader {
    pub fn new(client: reqwest::Client, runtime: tokio::runtime::Handle, config: &Config) -> Self {
        Self {
            client,
            runtime,
            lookup_cache: SentryLookupCache::builder()
                .max_capacity(config.caches.in_memory.sentry_index_capacity)
                .time_to_live(config.caches.in_memory.sentry_index_ttl)
                .build(),
            connect_timeout: config.connect_timeout,
            streaming_timeout: config.streaming_timeout,
        }
    }

    /// Make a request to Sentry and parse the result as JSON.
    async fn fetch_sentry_json<T>(
        client: &reqwest::Client,
        query: &SearchQuery,
    ) -> CacheEntry<Vec<T>>
    where
        T: DeserializeOwned,
    {
        let mut request = client
            .get(query.query_url.clone())
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
            tracing::trace!("Success fetching index from Sentry");
            Ok(response.json().await?)
        } else {
            tracing::warn!("Sentry returned status code {}", response.status());
            let details = response.status().to_string();
            Err(CacheError::DownloadError(details))
        }
    }

    /// Return the search results.
    ///
    /// If there are cached search results this skips the actual search.
    async fn cached_sentry_search<T>(&self, query: SearchQuery) -> CacheEntry<SearchResult>
    where
        T: IntoSearchResult + DeserializeOwned + Send + 'static,
    {
        metric!(counter("source.sentry.query.access") += 1);

        let query_ = query.clone();
        let init = Box::pin(async {
            metric!(counter("source.sentry.query.computation") += 1);
            tracing::debug!(
                "Fetching list of Sentry debug files from {}",
                &query_.query_url
            );

            let client = self.client.clone();
            let future =
                async move { super::retry(|| Self::fetch_sentry_json(&client, &query_)).await };

            let future =
                CancelOnDrop::new(self.runtime.spawn(future.bind_hub(sentry::Hub::current())));

            let vec = future.await.map_err(|_| CacheError::InternalError)??;
            Ok(T::into_search_result(vec))
        });

        self.lookup_cache
            .entry(query)
            .or_insert_with_if(init, |entry| entry.is_err())
            .await
            .into_value()
    }

    pub async fn list_files(
        &self,
        source: Arc<SentrySourceConfig>,
        object_id: &ObjectId,
        file_types: &[FileType],
    ) -> CacheEntry<Vec<RemoteFile>> {
        // TODO(flub): These queries do not handle pagination.  But sentry only starts to
        // paginate at 20 results so we get away with this for now.

        // There needs to be either a debug_id or a code_id filter in the query. Otherwise, this would
        // return a list of all debug files in the project.
        if object_id.debug_id.is_none() && object_id.code_id.is_none() {
            return Ok(Vec::new());
        }

        let mut query_url = source.url.clone();
        if let Some(ref debug_id) = object_id.debug_id {
            query_url
                .query_pairs_mut()
                .append_pair("debug_id", &debug_id.to_string());
        }

        // See <sentry-repo>/src/sentry/constants.py KNOWN_DIF_FORMATS for these query strings.
        query_url.query_pairs_mut().extend_pairs(
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
                    FileType::Il2cpp => "il2cpp",
                    FileType::PortablePdb => "portablepdb",
                })
                .map(|val| ("file_formats", val)),
        );

        if let Some(ref code_id) = object_id.code_id {
            query_url
                .query_pairs_mut()
                .append_pair("code_id", code_id.as_str());
        }

        let query = SearchQuery {
            query_url,
            token: source.token.clone(),
        };

        let search_result = self.cached_sentry_search::<JustIdResult>(query).await?;
        let results = match search_result {
            SearchResult::JustId(results) => results,
            SearchResult::Artifact(_) => {
                tracing::error!("Cached `SearchResult` has wrong inner type, expected `JustId`");
                return Err(CacheError::InternalError);
            }
        };

        let file_ids = results
            .into_iter()
            .map(|search_result| {
                SentryRemoteFile::new(source.clone(), search_result.id, SentryFileType::DebugFile)
                    .into()
            })
            .collect();

        Ok(file_ids)
    }

    pub async fn list_artifacts(
        &self,
        source: Arc<SentrySourceConfig>,
        file_stems: BTreeSet<String>,
    ) -> CacheEntry<Vec<ArtifactResult>> {
        let mut query_url = source.url.clone();

        // Pre-filter required artifacts, so it limits number of pages we have to fetch
        // in order to collect all the necessary data.
        for stem in file_stems {
            query_url.query_pairs_mut().append_pair("query", &stem);
        }

        let query = SearchQuery {
            query_url,
            token: source.token.clone(),
        };

        let search_result = self.cached_sentry_search::<ArtifactResult>(query).await?;
        let entries = match search_result {
            SearchResult::Artifact(entries) => entries,
            SearchResult::JustId(_) => {
                tracing::error!("Cached `SearchResult` has wrong inner type, expected `Artifact`");
                return Err(CacheError::InternalError);
            }
        };

        Ok(entries)
    }

    /// Downloads a source hosted on Sentry.
    pub async fn download_source(
        &self,
        file_source: SentryRemoteFile,
        destination: &Path,
    ) -> CacheEntry {
        tracing::debug!("Fetching Sentry artifact from {}", file_source.url());

        let request = self
            .client
            .get(file_source.url())
            .header("User-Agent", USER_AGENT)
            .bearer_auth(&file_source.source.token);
        let source = RemoteFile::from(file_source);

        super::download_reqwest(
            &source,
            request,
            self.connect_timeout,
            self.streaming_timeout,
            destination,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use symbolicator_sources::{RemoteFileUri, SourceId};

    #[test]
    fn test_download_url() {
        let source = SentrySourceConfig {
            id: SourceId::new("test"),
            url: Url::parse("https://example.net/endpoint/").unwrap(),
            token: "token".into(),
        };
        let file_source = SentryRemoteFile::new(
            Arc::new(source),
            SentryFileId("abc123".into()),
            SentryFileType::DebugFile,
        );
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
        let file_source = SentryRemoteFile::new(
            Arc::new(source),
            SentryFileId("abc123".into()),
            SentryFileType::DebugFile,
        );
        let uri = file_source.uri();
        assert_eq!(
            uri,
            RemoteFileUri::new("sentry://project_debug_file/abc123")
        );
    }
}
