//! Support to download from sentry sources.
//!
//! This allows to fetch files which were directly uploaded to Sentry itself.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use sentry::SentryFutureExt;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::fs::File;
use url::Url;

use symbolicator_sources::{
    ObjectId, RemoteFile, SentryFileId, SentryRemoteFile, SentrySourceConfig,
};

use super::{FileType, USER_AGENT};
use crate::caching::{CacheEntry, CacheError};
use crate::config::InMemoryCacheConfig;
use crate::utils::futures::{m, measure, CancelOnDrop};
use crate::utils::http::DownloadTimeouts;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    id: SentryFileId,
    symbol_type: SentryFileType,
}

/// The is almost the same as [`FileType`], except it does not treat MachO, Elf and Wasm
/// Code / Debug differently.
/// All the formats Sentry itself knows about are listed here:
/// <https://github.com/getsentry/sentry/blob/8fd506a8af5264b1f894fcf7ad066faf64cb966c/src/sentry/constants.py#L315-L329>
#[derive(Copy, Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum SentryFileType {
    Pe,
    Pdb,
    PortablePdb,
    MachO,
    Elf,
    Wasm,
    Breakpad,
    SourceBundle,
    UuidMap,
    BcSymbolMap,
    Il2cpp,
    Proguard,
}

impl From<FileType> for SentryFileType {
    fn from(file_type: FileType) -> Self {
        match file_type {
            FileType::Pe => Self::Pe,
            FileType::Pdb => Self::Pdb,
            FileType::PortablePdb => Self::PortablePdb,
            FileType::MachDebug | FileType::MachCode => Self::MachO,
            FileType::ElfDebug | FileType::ElfCode => Self::Elf,
            FileType::WasmDebug | FileType::WasmCode => Self::Wasm,
            FileType::Breakpad => Self::Breakpad,
            FileType::SourceBundle => Self::SourceBundle,
            FileType::UuidMap => Self::UuidMap,
            FileType::BcSymbolMap => Self::BcSymbolMap,
            FileType::Il2cpp => Self::Il2cpp,
            FileType::Proguard => Self::Proguard,
        }
    }
}

impl SentryFileType {
    fn matches(self, file_types: &[FileType]) -> bool {
        file_types.iter().any(|ty| Self::from(*ty) == self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SearchQuery {
    pub index_url: Url,
    pub token: String,
}

/// An LRU Cache for Sentry DIF (Native Debug Files) lookups.
type SentryDifCache = moka::future::Cache<SearchQuery, CacheEntry<Arc<[SearchResult]>>>;

pub struct SentryDownloader {
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
    dif_cache: SentryDifCache,
    timeouts: DownloadTimeouts,
    propagate_traces: bool,
}

impl fmt::Debug for SentryDownloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SentryDownloader")
            .field("dif_cache", &self.dif_cache.entry_count())
            .field("timeouts", &self.timeouts)
            .field("propagate_traces", &self.propagate_traces)
            .finish()
    }
}

impl SentryDownloader {
    pub fn new(
        client: reqwest::Client,
        runtime: tokio::runtime::Handle,
        timeouts: DownloadTimeouts,
        in_memory: &InMemoryCacheConfig,
        propagate_traces: bool,
    ) -> Self {
        let dif_cache = SentryDifCache::builder()
            .max_capacity(in_memory.sentry_index_capacity)
            .time_to_live(in_memory.sentry_index_ttl)
            .build();
        Self {
            client,
            runtime,
            dif_cache,
            timeouts,
            propagate_traces,
        }
    }

    /// Make a request to sentry, parse the result as a JSON SearchResult list.
    #[tracing::instrument(skip_all)]
    pub async fn fetch_sentry_json<T>(
        client: &reqwest::Client,
        query: &SearchQuery,
        propagate_traces: bool,
    ) -> CacheEntry<Arc<[T]>>
    where
        T: DeserializeOwned,
    {
        let mut request = client
            .get(query.index_url.clone())
            .bearer_auth(&query.token)
            .header("Accept-Encoding", "identity")
            .header("User-Agent", USER_AGENT);

        if propagate_traces {
            if let Some(span) = sentry::configure_scope(|scope| scope.get_span()) {
                for (k, v) in span.iter_headers() {
                    request = request.header(k, v);
                }
            }
        }

        let response = request.send().await?;

        if response.status().is_success() {
            tracing::trace!("Success fetching from Sentry API");
            Ok(response.json().await?)
        } else {
            tracing::warn!("Sentry API returned status code {}", response.status());
            let details = response.status().to_string();
            Err(CacheError::DownloadError(details))
        }
    }

    pub async fn list_files(
        &self,
        source: Arc<SentrySourceConfig>,
        object_id: &ObjectId,
        file_types: &[FileType],
    ) -> CacheEntry<Vec<RemoteFile>> {
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
        } else if let Some(ref code_id) = object_id.code_id {
            // NOTE: We only query by code-id if we do *not* have a debug-id.
            // This matches the Sentry backend behavior here:
            // <https://github.com/getsentry/sentry/blob/17644550024d6a2eb01356ee48ec0d3ef95c043d/src/sentry/api/endpoints/debug_files.py#L155-L161>
            index_url
                .query_pairs_mut()
                .append_pair("code_id", code_id.as_str());
        }

        // NOTE: We intentionally don't limit the query to the provided file types, even though
        // the endpoint supports it. The reason is that the result of the query gets cached locally
        // and we can then filter the cached results. This saves us from making individual requests to Sentry
        // for every file type or combination of file types we need.
        let query = SearchQuery {
            index_url,
            token: source.token.clone(),
        };

        metric!(counter("source.sentry.dif_query.access") += 1);

        let init = Box::pin(async {
            metric!(counter("source.sentry.dif_query.computation") += 1);
            tracing::debug!(
                "Fetching list of Sentry debug files from {}",
                &query.index_url
            );

            let future = {
                let client = self.client.clone();
                let query = query.clone();
                let propagate_traces = self.propagate_traces;
                async move {
                    super::retry(|| Self::fetch_sentry_json(&client, &query, propagate_traces))
                        .await
                }
            };

            let future =
                CancelOnDrop::new(self.runtime.spawn(future.bind_hub(sentry::Hub::current())));

            let timeout = Duration::from_secs(30);
            let future = tokio::time::timeout(timeout, future);
            let future = measure("service.download.list_files", m::timed_result, future);

            let result = future
                .await
                .map_err(|_| CacheError::Timeout(timeout))?
                .map_err(|_| CacheError::InternalError)?;

            if let Ok(result) = &result {
                // TODO(flub): These queries do not handle pagination.  But sentry only starts to
                // paginate at 20 results so we get away with this for now.
                if result.len() >= 20 {
                    tracing::error!(query = ?query.index_url, "Sentry API Query returned 20 results");
                }
            }

            result
        });

        let entries = self
            .dif_cache
            .entry_by_ref(&query)
            .or_insert_with_if(init, |entry| entry.is_err())
            .await
            .into_value()?;

        let file_ids = entries
            .iter()
            .filter(|file| file.symbol_type.matches(file_types))
            .map(|file| SentryRemoteFile::new(source.clone(), true, file.id.clone(), None).into())
            .collect();
        Ok(file_ids)
    }

    /// Downloads a source hosted on Sentry.
    pub async fn download_source(
        &self,
        source_name: &str,
        file_source: &SentryRemoteFile,
        destination: &mut File,
    ) -> CacheEntry {
        let url = file_source.url();
        tracing::debug!("Fetching Sentry artifact from {}", url);

        let mut builder = self.client.get(url).header("User-Agent", USER_AGENT);
        if file_source.use_credentials() {
            builder = builder.bearer_auth(&file_source.source.token);
        }

        super::download_reqwest(source_name, builder, &self.timeouts, destination).await
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
        let file_source =
            SentryRemoteFile::new(Arc::new(source), true, SentryFileId("abc123".into()), None);
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
        let file_source =
            SentryRemoteFile::new(Arc::new(source), true, SentryFileId("abc123".into()), None);
        let uri = file_source.uri();
        assert_eq!(
            uri,
            RemoteFileUri::new("sentry://project_debug_file/abc123")
        );
    }
}
