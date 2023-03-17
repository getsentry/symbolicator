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
    HttpRemoteFile, ObjectId, RemoteFile, SentryFileId, SentryRemoteFile, SentrySourceConfig,
};

use super::{FileType, USER_AGENT};
use crate::caching::{CacheEntry, CacheError};
use crate::config::Config;
use crate::utils::futures::CancelOnDrop;

#[derive(Clone, Debug, Deserialize)]
struct SearchResult {
    pub id: SentryFileId,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SearchArtifactResult {
    pub id: SentryFileId,
    pub name: String,
    pub sha1: String,
    pub dist: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
enum RawJsLookupResult {
    Bundle {
        id: SentryFileId,
        url: Url,
    },
    File {
        id: SentryFileId,
        url: Url,
        abs_path: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// The Result of looking up JS Artifacts.
#[derive(Clone, Debug)]
pub enum JsLookupResult {
    /// This is an `ArtifactBundle`.
    Bundle {
        /// The [`RemoteFile`] to download this bundle from.
        remote_file: RemoteFile,
    },
    /// This is an individual artifact file.
    Individual {
        /// The [`RemoteFile`] to download this artifact from.
        remote_file: RemoteFile,
        /// The absolute path (also called `url`) of the artifact.
        abs_path: String,
        /// Arbitrary headers of this file, such as a `Sourcemap` reference.
        headers: BTreeMap<String, String>,
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
    connect_timeout: Duration,
    streaming_timeout: Duration,
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
    pub fn new(client: reqwest::Client, runtime: tokio::runtime::Handle, config: &Config) -> Self {
        let dif_cache = SentryDifCache::builder()
            .max_capacity(config.caches.in_memory.sentry_index_capacity)
            .time_to_live(config.caches.in_memory.sentry_index_ttl)
            .build();
        let js_cache = SentryJsCache::builder()
            .max_capacity(config.caches.in_memory.sentry_index_capacity)
            .time_to_live(config.caches.in_memory.sentry_index_ttl)
            .build();
        Self {
            client,
            runtime,
            dif_cache,
            js_cache,
            connect_timeout: config.connect_timeout,
            streaming_timeout: config.streaming_timeout,
        }
    }

    /// Make a request to sentry, parse the result as a JSON SearchResult list.
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
    async fn cached_sentry_search(&self, query: SearchQuery) -> CacheEntry<Arc<[SearchResult]>> {
        metric!(counter("source.sentry.dif_query.access") += 1);

        let query_ = query.clone();
        let init = Box::pin(async {
            metric!(counter("source.sentry.dif_query.computation") += 1);
            tracing::debug!(
                "Fetching list of Sentry debug files from {}",
                &query_.index_url
            );

            let client = self.client.clone();
            let future =
                async move { super::retry(|| Self::fetch_sentry_json(&client, &query_)).await };

            let future =
                CancelOnDrop::new(self.runtime.spawn(future.bind_hub(sentry::Hub::current())));

            future.await.map_err(|_| CacheError::InternalError)?
        });

        self.dif_cache
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
                    FileType::Il2cpp => "il2cpp",
                    FileType::PortablePdb => "portablepdb",
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

        let search = self.cached_sentry_search(query).await?;
        let file_ids = search
            .iter()
            .map(|search_result| {
                SentryRemoteFile::new(source.clone(), search_result.id.clone(), None).into()
            })
            .collect();

        Ok(file_ids)
    }

    pub async fn list_artifacts(
        &self,
        source: Arc<SentrySourceConfig>,
        file_stems: BTreeSet<String>,
    ) -> CacheEntry<Vec<SearchArtifactResult>> {
        let mut index_url = source.url.clone();

        // Pre-filter required artifacts, so it limits number of pages we have to fetch
        // in order to collect all the necessary data.
        for stem in file_stems {
            index_url.query_pairs_mut().append_pair("query", &stem);
        }

        let query = SearchQuery {
            index_url,
            token: source.token.clone(),
        };

        tracing::debug!(
            "Fetching list of Sentry artifacts from {}",
            &query.index_url
        );

        let entries = {
            let client = self.client.clone();
            let query = query.clone();
            let future =
                async move { super::retry(|| Self::fetch_sentry_json(&client, &query)).await };

            let future =
                CancelOnDrop::new(self.runtime.spawn(future.bind_hub(sentry::Hub::current())));

            future.await.map_err(|_| CacheError::InternalError)??
        };

        Ok(entries.iter().cloned().collect())
    }

    // TODO: this should completely replace the `list_artifacts` above
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
            }
            if let Some(dist) = dist {
                query.append_pair("dist", dist);
            }
            for debug_id in debug_ids {
                query.append_pair("debug_id", &debug_id.to_string());
            }
            for file_stem in file_stems {
                query.append_pair("url", &file_stem);
            }
        }

        let query = SearchQuery {
            index_url: lookup_url,
            token: source.token.clone(),
        };

        metric!(counter("source.sentry.js_lookup.access") += 1);

        let query_ = query.clone();

        let init = Box::pin(async {
            metric!(counter("source.sentry.js_lookup.computation") += 1);
            tracing::debug!(
                "Fetching list of Sentry JS artifacts from {}",
                &query_.index_url
            );

            let client = self.client.clone();
            let future =
                async move { super::retry(|| Self::fetch_sentry_json(&client, &query_)).await };

            let future =
                CancelOnDrop::new(self.runtime.spawn(future.bind_hub(sentry::Hub::current())));

            future.await.map_err(|_| CacheError::InternalError)?
        });

        let entries = self
            .js_cache
            .entry(query)
            .or_insert_with_if(init, |entry| entry.is_err())
            .await
            .into_value()?;

        Ok(entries
            .iter()
            .map(|raw| match raw {
                RawJsLookupResult::Bundle { id, url } => JsLookupResult::Bundle {
                    remote_file: make_remote_file(&source, id, url),
                },
                RawJsLookupResult::File {
                    id,
                    url,
                    abs_path,
                    headers,
                } => JsLookupResult::Individual {
                    remote_file: make_remote_file(&source, id, url),
                    abs_path: abs_path.clone(),
                    headers: headers.clone(),
                },
            })
            .collect())
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

/// Transforms the gives `url` into a [`RemoteFile`].
///
/// Depending on the `source`, this creates either a [`SentryRemoteFile`], or a
/// [`HttpRemoteFile`].
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
    if url.as_str().starts_with(source.url.as_str()) {
        SentryRemoteFile::new(Arc::clone(source), file_id.clone(), Some(url.clone())).into()
    } else {
        HttpRemoteFile::from_url(url.clone()).into()
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
            SentryRemoteFile::new(Arc::new(source), SentryFileId("abc123".into()), None);
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
            SentryRemoteFile::new(Arc::new(source), SentryFileId("abc123".into()), None);
        let uri = file_source.uri();
        assert_eq!(
            uri,
            RemoteFileUri::new("sentry://project_debug_file/abc123")
        );
    }
}
