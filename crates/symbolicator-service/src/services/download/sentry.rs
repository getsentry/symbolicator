//! Support to download from sentry sources.
//!
//! This allows to fetch files which were directly uploaded to Sentry itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sentry::types::DebugId;
use sentry::SentryFutureExt;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use symbolicator_sources::{
    ObjectId, RemoteFile, SentryFileId, SentryRemoteFile, SentrySourceConfig,
};

use super::{FileType, USER_AGENT};
use crate::caching::{CacheEntry, CacheError};
use crate::config::InMemoryCacheConfig;
use crate::types::ResolvedWith;
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
        }
    }
}

impl SentryFileType {
    fn matches(self, file_types: &[FileType]) -> bool {
        file_types.iter().any(|ty| Self::from(*ty) == self)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawJsLookupResult {
    Bundle {
        id: SentryFileId,
        url: Url,
        resolved_with: ResolvedWith,
    },
    File {
        id: SentryFileId,
        url: Url,
        abs_path: String,
        #[serde(default)]
        headers: ArtifactHeaders,
        resolved_with: ResolvedWith,
    },
}

pub type ArtifactHeaders = BTreeMap<String, String>;

/// The Result of looking up JS Artifacts.
#[derive(Clone, Debug)]
pub enum JsLookupResult {
    /// This is an `ArtifactBundle`.
    ArtifactBundle {
        /// The [`RemoteFile`] to download this bundle from.
        remote_file: RemoteFile,
        resolved_with: ResolvedWith,
    },
    /// This is an individual artifact file.
    IndividualArtifact {
        /// The [`RemoteFile`] to download this artifact from.
        remote_file: RemoteFile,
        /// The absolute path (also called `url`) of the artifact.
        abs_path: String,
        /// Arbitrary headers of this file, such as a `Sourcemap` reference.
        headers: ArtifactHeaders,
        resolved_with: ResolvedWith,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SearchQuery {
    index_url: Url,
    token: String,
}

/// An LRU Cache for Sentry DIF (Native Debug Files) lookups.
type SentryDifCache = moka::future::Cache<SearchQuery, CacheEntry<Arc<[SearchResult]>>>;

/// An LRU Cache for Sentry JS Artifact lookups.
type SentryJsCache = moka::future::Cache<SearchQuery, CacheEntry<Arc<[RawJsLookupResult]>>>;

pub struct SentryDownloader {
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
    dif_cache: SentryDifCache,
    js_cache: SentryJsCache,
    timeouts: DownloadTimeouts,
}

impl fmt::Debug for SentryDownloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SentryDownloader")
            .field("dif_cache", &self.dif_cache.entry_count())
            .field("js_cache", &self.js_cache.entry_count())
            .finish()
    }
}

impl SentryDownloader {
    pub fn new(
        client: reqwest::Client,
        runtime: tokio::runtime::Handle,
        timeouts: DownloadTimeouts,
        in_memory: &InMemoryCacheConfig,
    ) -> Self {
        let dif_cache = SentryDifCache::builder()
            .max_capacity(in_memory.sentry_index_capacity)
            .time_to_live(in_memory.sentry_index_ttl)
            .build();
        let js_cache = SentryJsCache::builder()
            .max_capacity(in_memory.sentry_index_capacity)
            .time_to_live(in_memory.sentry_index_ttl)
            .build();
        Self {
            client,
            runtime,
            dif_cache,
            js_cache,
            timeouts,
        }
    }

    /// Make a request to sentry, parse the result as a JSON SearchResult list.
    #[tracing::instrument(skip_all)]
    async fn fetch_sentry_json<T>(
        client: &reqwest::Client,
        query: &SearchQuery,
    ) -> CacheEntry<Arc<[T]>>
    where
        T: DeserializeOwned,
    {
        let mut request = client
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
                async move { super::retry(|| Self::fetch_sentry_json(&client, &query)).await }
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

    /// Look up a list of bundles or individual artifact files covering the
    /// `debug_ids` and `file_stems` (using the `release` + `dist`).
    pub async fn lookup_js_artifacts(
        &self,
        source: Arc<SentrySourceConfig>,
        debug_ids: BTreeSet<DebugId>,
        file_stems: BTreeSet<String>,
        release: Option<&str>,
        dist: Option<&str>,
    ) -> CacheEntry<Vec<JsLookupResult>> {
        let mut lookup_url = source.url.clone();
        {
            let mut query = lookup_url.query_pairs_mut();

            if let Some(release) = release {
                query.append_pair("release", release);

                // A `url` is only valid in combination with a `release`.
                for file_stem in file_stems {
                    query.append_pair("url", &file_stem);
                }
            }
            if let Some(dist) = dist {
                query.append_pair("dist", dist);
            }
            for debug_id in debug_ids {
                query.append_pair("debug_id", &debug_id.to_string());
            }
        }

        // NOTE: `http::Uri` has a hard limit defined, and reqwest unconditionally unwraps such
        // errors, when converting between `Url` to `Uri`. To avoid a panic in that case, we
        // duplicate the check here to gracefully error out.
        if lookup_url.as_str().len() > (u16::MAX - 1) as usize {
            return Err(CacheError::DownloadError("uri too long".into()));
        }

        let query = SearchQuery {
            index_url: lookup_url,
            token: source.token.clone(),
        };

        metric!(counter("source.sentry.js_lookup.access") += 1);

        let init = Box::pin(async {
            metric!(counter("source.sentry.js_lookup.computation") += 1);
            tracing::debug!(
                "Fetching list of Sentry JS artifacts from {}",
                &query.index_url
            );

            let future = {
                let client = self.client.clone();
                let query = query.clone();
                async move { super::retry(|| Self::fetch_sentry_json(&client, &query)).await }
            };

            let future =
                CancelOnDrop::new(self.runtime.spawn(future.bind_hub(sentry::Hub::current())));

            let timeout = Duration::from_secs(30);
            let future = tokio::time::timeout(timeout, future);
            let future = measure(
                "service.download.lookup_js_artifacts",
                m::timed_result,
                future,
            );

            future
                .await
                .map_err(|_| CacheError::Timeout(timeout))?
                .map_err(|_| CacheError::InternalError)?
        });

        let entries = self
            .js_cache
            .entry_by_ref(&query)
            .or_insert_with_if(init, |entry| entry.is_err())
            .await
            .into_value()?;

        let results = entries
            .iter()
            .map(|raw| match raw {
                RawJsLookupResult::Bundle {
                    id,
                    url,
                    resolved_with,
                } => JsLookupResult::ArtifactBundle {
                    remote_file: make_remote_file(&source, id, url),
                    resolved_with: *resolved_with,
                },
                RawJsLookupResult::File {
                    id,
                    url,
                    abs_path,
                    headers,
                    resolved_with,
                } => JsLookupResult::IndividualArtifact {
                    remote_file: make_remote_file(&source, id, url),
                    abs_path: abs_path.clone(),
                    headers: headers.clone(),
                    resolved_with: *resolved_with,
                },
            })
            .collect();
        Ok(results)
    }

    /// Downloads a source hosted on Sentry.
    pub async fn download_source(
        &self,
        file_source: SentryRemoteFile,
        destination: &Path,
    ) -> CacheEntry {
        tracing::debug!("Fetching Sentry artifact from {}", file_source.url());

        let mut request = self
            .client
            .get(file_source.url())
            .header("User-Agent", USER_AGENT);
        if file_source.use_credentials() {
            request = request.bearer_auth(&file_source.source.token);
        }
        let source = RemoteFile::from(file_source);

        super::download_reqwest(&source, request, &self.timeouts, destination).await
    }
}

/// Transforms the given `url` into a [`RemoteFile`].
///
/// The problem here is being forward-compatible to a future in which the Sentry API returns
/// pre-authenticated Urls on some external file storage service.
/// Whereas right now, these files are still being served from a Sentry API endpoint, which
/// needs to be authenticated via a `token` that we do not want to leak to any public Url, as
/// well as using a restricted IP that is being blocked for arbitrary HTTP files.
fn make_remote_file(
    source: &Arc<SentrySourceConfig>,
    file_id: &SentryFileId,
    url: &Url,
) -> RemoteFile {
    let use_credentials = url.as_str().starts_with(source.url.as_str());
    SentryRemoteFile::new(
        Arc::clone(source),
        use_credentials,
        file_id.clone(),
        Some(url.clone()),
    )
    .into()
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
