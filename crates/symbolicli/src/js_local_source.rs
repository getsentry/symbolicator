use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::routing::{get, get_service};
use axum::{Json, Router, extract};
use serde::{Deserialize, Serialize};
use symbolic::common::DebugId;
use symbolic::debuginfo::sourcebundle::{SourceFileInfo, SourceFileType};
use symbolicator_js::interface::ResolvedWith;
use symbolicator_sources::{SentrySourceConfig, SentryToken, SourceId};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use url::Url;
use walkdir::WalkDir;
use zip::ZipArchive;

/// Start a server that mimicks the Sentry artifact lookup endpoint, serving artifact
/// bundles from the given directory.
pub fn start_server(path: impl AsRef<Path> + Clone) -> anyhow::Result<SentrySourceConfig> {
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener = TcpListener::bind(addr).unwrap();
    listener.set_nonblocking(true).unwrap();
    let socket = listener.local_addr().unwrap();
    let index = Index::new(path.clone(), socket)?;
    let index = Arc::new(index);
    let source_url = index.url("lookup");

    let router = Router::new()
        .route("/lookup", get(lookup))
        .nest_service("/bundles", get_service(ServeDir::new(path)))
        .layer(TraceLayer::new_for_http())
        .with_state(index);

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        axum::serve(listener, router).await.unwrap();
    });

    Ok(SentrySourceConfig {
        id: SourceId::new("local"),
        url: source_url,
        token: SentryToken(String::new()),
    })
}

/// A key with which to look up an artifact bundle.
///
/// This is deserialized from a query in [`lookup`].
#[derive(Debug, Clone, Deserialize)]
struct LookupKey {
    /// The release name.
    ///
    /// This is only relevant in conjunction with `url`.
    release: Option<Arc<str>>,
    /// The dist name.
    ///
    /// This is only relevant in conjunction with `url`.
    dist: Option<Arc<str>>,
    /// The URL/abs_path.
    url: Option<Arc<str>>,
    /// The debug ID.
    debug_id: Option<DebugId>,
}

/// A combination of release + dist + URL for
/// path-based lookups.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReleaseDistUrl {
    release: Option<Arc<str>>,
    dist: Option<Arc<str>>,
    url: Arc<str>,
}

/// Simple representation of the manifest of a sourcebundle (including artifact bundles.).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SourceBundleManifest {
    #[serde(default)]
    pub files: HashMap<String, SourceFileInfo>,
    #[serde(default)]
    pub attributes: HashMap<String, Arc<str>>,
}

/// Application state for the pretend Sentry source.
///
/// This is immutable after initial construction.
#[derive(Debug, Clone)]
struct Index {
    /// The socket on which the server listens.
    socket: SocketAddr,
    /// A map from debug IDs to bundles containing them.
    by_debug_id: HashMap<DebugId, HashSet<Arc<Path>>>,
    /// A map from release/dist/URL combinations to bundles containing them.
    by_url: HashMap<ReleaseDistUrl, HashSet<Arc<Path>>>,
}

impl Index {
    /// Build a new index containing bundles within the given `base_path`
    /// and serving downloads on the given `socket`.
    fn new(base_path: impl AsRef<Path>, socket: SocketAddr) -> Result<Self> {
        let base_path = base_path.as_ref();
        let mut out = Self {
            socket,
            by_debug_id: Default::default(),
            by_url: Default::default(),
        };

        for entry in WalkDir::new(base_path) {
            let entry = entry.context("Accessing files")?;

            if !entry.file_type().is_file()
                || entry.path().extension().is_none_or(|ext| ext != "zip")
            {
                continue;
            }

            let bundle_path: Arc<Path> = entry.path().into();
            let relative_path: Arc<Path> = bundle_path.strip_prefix(base_path).unwrap().into();

            let archive = std::fs::File::open(&bundle_path)?;
            let mut archive = ZipArchive::new(archive)?;

            let manifest = archive.by_name("manifest.json")?;
            let manifest: SourceBundleManifest = serde_json::from_reader(manifest)?;

            let release = manifest.attributes.get("release").cloned();
            let dist = manifest.attributes.get("dist").cloned();

            for file in manifest.files.values() {
                if file.ty() != Some(SourceFileType::MinifiedSource) {
                    continue;
                }

                let Some(url) = file.url() else {
                    continue;
                };

                out.by_url
                    .entry(ReleaseDistUrl {
                        release: release.clone(),
                        dist: dist.clone(),
                        url: url.into(),
                    })
                    .or_default()
                    .insert(Arc::clone(&relative_path));

                if let Some(debug_id) = file.debug_id() {
                    out.by_debug_id
                        .entry(debug_id.to_owned())
                        .or_default()
                        .insert(Arc::clone(&relative_path));
                }
            }
        }

        Ok(out)
    }

    /// Returns a full URL pointing to the given path.
    fn url(&self, path: &str) -> Url {
        let path = path.trim_start_matches('/');
        format!("http://{}/{}", self.socket, path).parse().unwrap()
    }
}

/// A value returned by artifact lookup.
///
/// Currently this only supports bundles.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LookupResult {
    Bundle {
        /// The bundle's ID.
        ///
        /// We use the lossy version of the file's path for this.
        id: String,
        /// The URL where the bundle can be downloaded.
        url: Url,
        /// How the bundle was resolved.
        resolved_with: ResolvedWith,
    },
}

async fn lookup(
    extract::State(index): extract::State<Arc<Index>>,
    extract::Query(key): extract::Query<LookupKey>,
) -> Json<Box<[LookupResult]>> {
    let mut out = Vec::new();
    let mut found_bundles = HashSet::new();

    if let Some(debug_id) = key.debug_id {
        for path in index
            .by_debug_id
            .get(&debug_id)
            .into_iter()
            .flat_map(|s| s.iter().cloned())
        {
            if found_bundles.contains(&path) {
                continue;
            }
            let path_encoded = urlencoding::encode_binary(path.as_os_str().as_encoded_bytes());
            out.push(LookupResult::Bundle {
                id: path.to_string_lossy().to_string(),
                url: index.url(&format!("bundles/{path_encoded}")),
                resolved_with: ResolvedWith::DebugId,
            });
            found_bundles.insert(path);
        }
    }

    if let Some(url) = key.url {
        for path in index
            .by_url
            .get(&ReleaseDistUrl {
                release: key.release.clone(),
                dist: key.dist.clone(),
                url: url.clone(),
            })
            .into_iter()
            .flat_map(|s| s.iter().cloned())
        {
            if found_bundles.contains(&path) {
                continue;
            }
            let path_encoded = urlencoding::encode_binary(path.as_os_str().as_encoded_bytes());
            out.push(LookupResult::Bundle {
                id: path.to_string_lossy().to_string(),
                url: index.url(&format!("bundles/{path_encoded}")),
                resolved_with: ResolvedWith::Release,
            });
            found_bundles.insert(path);
        }
    }

    Json(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_existing_bundle() {
        let source =
            start_server(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures")).unwrap();

        let mut lookup_url = source.url.clone();
        lookup_url.set_query(Some("debug_id=2f259f80-58b7-44cb-d7cd-de1505e7e718"));

        let results: Box<[LookupResult]> = reqwest::get(lookup_url)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let LookupResult::Bundle { url, .. } = &results[0];

        assert!(
            reqwest::get(url.clone())
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
}
