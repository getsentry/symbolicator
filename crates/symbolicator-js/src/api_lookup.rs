use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use sentry::types::DebugId;
use sentry::SentryFutureExt;
use serde::Deserialize;
use symbolicator_service::download::retry;
use symbolicator_service::download::sentry::{SearchQuery, SentryDownloader};
use symbolicator_service::metric;
use url::Url;

use symbolicator_service::caching::{CacheEntry, CacheError};
use symbolicator_service::config::InMemoryCacheConfig;
use symbolicator_service::utils::futures::{m, measure, CancelOnDrop};
use symbolicator_service::utils::http::DownloadTimeouts;
use symbolicator_sources::{RemoteFile, SentryFileId, SentryRemoteFile, SentrySourceConfig};

use crate::interface::ResolvedWith;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawJsLookupResult {
    Bundle {
        id: SentryFileId,
        url: Url,
        #[serde(default)]
        resolved_with: ResolvedWith,
    },
    File {
        id: SentryFileId,
        url: Url,
        abs_path: String,
        #[serde(default)]
        headers: ArtifactHeaders,
        #[serde(default)]
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

/// An LRU Cache for Sentry JS Artifact lookups.
type SentryJsCache = moka::future::Cache<SearchQuery, CacheEntry<Arc<[RawJsLookupResult]>>>;

pub struct SentryLookupApi {
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
    js_cache: SentryJsCache,
    timeouts: DownloadTimeouts,
    propagate_traces: bool,
}

impl fmt::Debug for SentryLookupApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SentryLookupApi")
            .field("js_cache", &self.js_cache.entry_count())
            .field("timeouts", &self.timeouts)
            .field("propagate_traces", &self.propagate_traces)
            .finish()
    }
}

impl SentryLookupApi {
    pub fn new(
        client: reqwest::Client,
        runtime: tokio::runtime::Handle,
        timeouts: DownloadTimeouts,
        in_memory: &InMemoryCacheConfig,
        propagate_traces: bool,
    ) -> Self {
        let js_cache = SentryJsCache::builder()
            .max_capacity(in_memory.sentry_index_capacity)
            .time_to_live(in_memory.sentry_index_ttl)
            .build();
        Self {
            client,
            runtime,
            js_cache,
            timeouts,
            propagate_traces,
        }
    }

    /// Look up a list of bundles or individual artifact files covering the
    /// `debug_ids` and `file_stems` (using the `release` + `dist`).
    #[tracing::instrument(skip(self, debug_ids, file_stems))]
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
                let propagate_traces = self.propagate_traces;
                async move {
                    retry(|| SentryDownloader::fetch_sentry_json(&client, &query, propagate_traces))
                        .await
                }
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
