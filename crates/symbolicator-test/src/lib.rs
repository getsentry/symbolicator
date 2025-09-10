//! Helpers for testing the web server and service.
//!
//! When writing tests, keep the following points in mind:
//!
//!  - In every test, call [`setup`]. This will set up the logger so that all console output
//!    is captured by the test runner.
//!
//!  - When using [`tempdir`], make sure that the handle to the temp directory is held for the
//!    entire lifetime of the test. When dropped too early, this might silently leak the temp
//!    directory, since symbolicator will create it again lazily after it has been deleted. To avoid
//!    this, assign it to a variable in the test function (e.g. `let _cache_dir = test::tempdir()`).
//!
//!  - When using [`symbol_server`], make sure that the server is held until all requests to
//!    the server have been made. If the server is dropped, the ports remain open and all
//!    connections to it will time out. To avoid this, assign it to a variable: `let (_server,
//!    source) = symbol_server();`. Alternatively, use [`local_source`] to test without
//!    HTTP connections.

use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, get_service};
use axum::{Json, extract};
use axum::{Router, middleware};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::fmt;

use symbolicator_sources::{
    CommonSourceConfig, DirectoryLayout, DirectoryLayoutType, FileType, FilesystemSourceConfig,
    GcsPrivateKey, GcsSourceKey, HttpSourceConfig, SentrySourceConfig, SentryToken, SourceConfig,
    SourceFilters, SourceId,
};

pub use tempfile::TempDir;

/// Setup the test environment.
///
///  - Initializes logs: The logger only captures logs from the `symbolicator` crate
///    and test server, and mutes all other logs.
///  - Initializes the default crypto provider.
pub fn setup() {
    // We depend on `rustls` with both the `aws-lc-rs` and
    // `ring` features enabled. This means that `rustls` can't automatically
    // decide which provider to use and we have to initialize it manually.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .unwrap();
    }

    fmt()
        .with_env_filter(EnvFilter::new("symbolicator=trace,tower_http=trace"))
        .with_target(false)
        .pretty()
        .with_test_writer()
        .try_init()
        .ok();
}

/// Creates a temporary directory.
///
/// The directory is deleted when the [`TempDir`] instance is dropped, unless
/// [`into_path`](TempDir::into_path) is called. Use it as a guard to automatically clean up after
/// tests.
pub fn tempdir() -> TempDir {
    TempDir::new().unwrap()
}

/// Returns the absolute path to the given fixture.
///
/// Fixtures are located in the `tests/fixtures` directory, located from the workspace root.
/// Fixtures can be either files, or directories.
///
/// # Panics
///
/// Panics if the fixture path does not exist on the file system.
#[track_caller]
pub fn fixture(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();

    let mut full_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    full_path.pop(); // to /crates/
    full_path.pop(); // to /
    full_path.push("./tests/fixtures/");
    full_path.push(path);

    assert!(full_path.exists(), "'{}' does not exist", path.display());

    full_path
}

/// Returns the contents of a fixture.
///
/// Fixtures are located in the `tests/fixtures` directory, located from the workspace root. The
/// fixture must be a readable file.
///
/// # Panics
///
/// Panics if the fixture does not exist or cannot be read.
pub fn read_fixture(path: impl AsRef<Path>) -> Vec<u8> {
    std::fs::read(fixture(path)).unwrap()
}

/// Get bucket configuration for the local fixtures.
///
/// Files are served directly via the local file system without the indirection through a HTTP
/// symbol server. This is the fastest way for testing, but also avoids certain code paths.
pub fn local_source() -> SourceConfig {
    SourceConfig::Filesystem(Arc::new(FilesystemSourceConfig {
        id: SourceId::new("local"),
        path: fixture("symbols"),
        files: Default::default(),
    }))
}

/// Get bucket configuration for the microsoft symbol server.
// FIXME(swatinem): this is used for a couple of http endpoint tests, which we could migrate to our
// local source as well.
pub fn microsoft_symsrv() -> SourceConfig {
    SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("microsoft"),
        url: "https://msdl.microsoft.com/download/symbols/"
            .parse()
            .unwrap(),
        headers: Default::default(),
        files: CommonSourceConfig {
            filters: SourceFilters {
                filetypes: vec![FileType::Pe, FileType::Pdb],
                ..Default::default()
            },
            ..Default::default()
        },
        accept_invalid_certs: false,
    }))
}

/// Gives a [`CommonSourceConfig`] with the given layout type and file types filter.
pub fn source_config(ty: DirectoryLayoutType, filetypes: Vec<FileType>) -> CommonSourceConfig {
    CommonSourceConfig {
        filters: SourceFilters {
            filetypes,
            ..Default::default()
        },
        layout: DirectoryLayout {
            ty,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A test server that binds to a random port and serves a web app.
///
/// The server counts all the requests that happen, to be accessed via `accesses` or `all_hits`.
///
/// It has a couple of routes with different behavior:
///
/// - `/redirect/$path` will redirect to the `$path` url.
/// - `/delay/$time/$path` will sleep for `$time` and then redirect to `$path`.
/// - `/msdl/` will redirect to the public microsoft symbol server.
/// - `/respond_statuscode/$num` responds with the status code given in `$num`.
/// - `/garbage_data/$data` responds back with `$data`.
/// - `/symbols/` serves the fixtures symbols.
///
/// This server requires a `tokio` runtime and is supposed to be run in a `tokio::test`. It
/// automatically stops serving when dropped.
#[derive(Debug)]
pub struct Server {
    handle: tokio::task::JoinHandle<()>,
    socket: SocketAddr,
    hits: Arc<Mutex<BTreeMap<String, usize>>>,
}

impl Server {
    /// Creates a new Server with a special testing-focused router,
    /// as described in the main [`Server`] docs.
    pub fn new() -> Self {
        Self::with_router(Self::test_router())
    }

    /// Creates a new Server with the given [`Router`].
    pub fn with_router(router: Router) -> Self {
        let hits = Arc::new(Mutex::new(BTreeMap::new()));

        let hitcounter = {
            let hits = hits.clone();
            move |extract::OriginalUri(uri), req, next: middleware::Next| {
                let hits = hits.clone();
                async move {
                    {
                        let mut hits = hits.lock().unwrap();
                        let hits = hits.entry(uri.to_string()).or_default();
                        *hits += 1;
                    }

                    next.run(req).await
                }
            }
        };

        let router = router.layer(middleware::from_fn(hitcounter));

        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let socket = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, router).await.unwrap();
        });

        Self {
            handle,
            socket,
            hits,
        }
    }

    /// Creates a new [`Router`] with the configuration as described in the main [`Server`] docs.
    pub fn test_router() -> Router {
        let serve_dir = get_service(ServeDir::new(fixture("symbols")));

        Router::new()
            .route(
                "/redirect/{*path}",
                get(|extract::Path(path): extract::Path<String>| async move {
                    (StatusCode::FOUND, [("Location", format!("/{path}"))])
                }),
            )
            .route(
                "/delay/{time}/{*path}",
                get(
                    |extract::Path((time, path)): extract::Path<(String, String)>| async move {
                        let duration = humantime::parse_duration(&time).unwrap();
                        tokio::time::sleep(duration).await;

                        (StatusCode::FOUND, [("Location", format!("/{path}"))])
                    },
                ),
            )
            .route(
                "/msdl/{*path}",
                get(|extract::Path(path): extract::Path<String>| async move {
                    let url = format!("https://msdl.microsoft.com/download/symbols/{path}");
                    (StatusCode::FOUND, [("Location", url)])
                }),
            )
            .route(
                "/respond_statuscode/{num}/{*tail}",
                get(
                    |extract::Path((num, _)): extract::Path<(u16, String)>| async move {
                        StatusCode::from_u16(num).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                    },
                ),
            )
            .route(
                "/garbage_data/{*tail}",
                get(|extract::Path(tail): extract::Path<String>| async move { tail }),
            )
            .nest_service("/symbols", serve_dir)
    }

    /// Returns the sum total of hits and clears the hit counts.
    pub fn accesses(&self) -> usize {
        let map = std::mem::take(&mut *self.hits.lock().unwrap());
        map.into_values().sum()
    }

    /// Returns a sorted list of `(path, hits)`-tuples, and clears the hit counts.
    pub fn all_hits(&self) -> Vec<(String, usize)> {
        let map = std::mem::take(&mut *self.hits.lock().unwrap());
        map.into_iter().collect()
    }

    /// Returns a full URL pointing to the given path.
    ///
    /// This URL uses `localhost` as hostname.
    pub fn url(&self, path: &str) -> Url {
        let path = path.trim_start_matches('/');
        format!("http://localhost:{}/{}", self.socket.port(), path)
            .parse()
            .unwrap()
    }

    /// Returns a [`SourceConfig`] hitting this server on the given `path`.
    pub fn source(&self, id: &str, path: &str) -> SourceConfig {
        let files = CommonSourceConfig {
            filters: SourceFilters {
                filetypes: vec![FileType::MachCode],
                path_patterns: vec![],
                requires_checksum: false,
            },
            layout: Default::default(),
            is_public: false,
            has_index: false,
        };
        self.source_with_config(id, path, files)
    }

    /// Returns a [`SourceConfig`] hitting this server on the given `path`,
    /// using the provided [`CommonSourceConfig`].
    pub fn source_with_config(
        &self,
        id: &str,
        path: &str,
        files: CommonSourceConfig,
    ) -> SourceConfig {
        SourceConfig::Http(Arc::new(HttpSourceConfig {
            id: SourceId::new(id),
            url: self.url(path),
            headers: Default::default(),
            files,
            accept_invalid_certs: false,
        }))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn an actual HTTP symbol server for local fixtures.
///
/// The symbol server serves static files from the local symbols fixture location under the
/// `/symbols` prefix. The layout of this folder is `DirectoryLayoutType::Native`. This function
/// returns the test server as well as a source configuration, which can be used to access the
/// symbol server in symbolication requests.
///
/// **Note**: The symbol server runs on localhost. By default, connections to local host are not
/// permitted, and need to be activated via `Config::connect_to_reserved_ips`.
pub fn symbol_server() -> (Server, SourceConfig) {
    let server = Server::new();

    // The source uses the same identifier ("local") as the local file system source to avoid
    // differences when changing the bucket in tests.
    let source = SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("local"),
        url: server.url("symbols/"),
        headers: Default::default(),
        files: Default::default(),
        accept_invalid_certs: false,
    }));

    (server, source)
}

pub fn sourcemap_server<L>(
    fixtures_dir: impl AsRef<Path>,
    lookup: L,
) -> (Server, SentrySourceConfig)
where
    L: Fn(&str, &str) -> serde_json::Value + Clone + Send + Sync + 'static,
{
    let files_url = Arc::new(OnceCell::<Url>::new());

    let router = {
        let files_url = files_url.clone();

        let tracing_layer = TraceLayer::new_for_http().on_response(());
        let serve_dir = get_service(ServeDir::new(fixtures_dir));
        Router::new()
            .route(
                "/lookup",
                get(move |extract::RawQuery(raw_query): extract::RawQuery| {
                    async move {
                        let files_url = files_url.get().unwrap().as_str();
                        let query = &raw_query.as_deref().unwrap_or_default();
                        // Its a bit unfortunate, but we went full circle back to constructing URLs
                        // by appending query parameters
                        if let Some((_, download_id)) = query.rsplit_once("download=") {
                            return Redirect::to(&format!("/files/{download_id}")).into_response();
                        }
                        let res = lookup(files_url, query);
                        Json(res).into_response()
                    }
                }),
            )
            .nest_service("/files", serve_dir)
            .layer(tracing_layer)
    };
    let server = Server::with_router(router);

    files_url.set(server.url("/files")).unwrap();

    let source = SentrySourceConfig {
        id: SourceId::new("sentry:project"),
        url: server.url("/lookup"),
        token: SentryToken(String::new()),
    };

    (server, source)
}

pub fn sentry_server<L>(fixtures_dir: impl AsRef<Path>, lookup: L) -> (Server, SentrySourceConfig)
where
    L: Fn(&str, &HashMap<String, String>) -> serde_json::Value + Clone + Send + Sync + 'static,
{
    let files_url = Arc::new(OnceCell::<Url>::new());

    let router = {
        let files_url = files_url.clone();

        let tracing_layer = TraceLayer::new_for_http().on_response(());
        let serve_dir = get_service(ServeDir::new(fixtures_dir));
        Router::new()
            .route(
                "/files/dsyms/",
                get(
                    move |extract::Query(params): extract::Query<HashMap<String, String>>| async move {
                        let files_url = files_url.get().unwrap().as_str();
                        if let Some(download_id) = params.get("id") {
                            return Redirect::to(&format!("/files/{download_id}")).into_response();
                        }
                        let res = lookup(files_url, &params);
                        Json(res).into_response()
                    },
                ),
            )
            .nest_service("/files", serve_dir)
            .layer(tracing_layer)
    };
    let server = Server::with_router(router);

    files_url.set(server.url("/files")).unwrap();

    let source = SentrySourceConfig {
        id: SourceId::new("sentry:project"),
        url: server.url("/files/dsyms/"),
        token: SentryToken(String::new()),
    };

    (server, source)
}

/// Returns the legacy read-only GCS credentials for testing GCS support.
///
/// Use the `gcs_source_key!()` macro instead which will skip correctly.
pub fn gcs_source_key_from_env() -> Option<GcsSourceKey> {
    let private_key = std::env::var("SENTRY_SYMBOLICATOR_GCS_PRIVATE_KEY").ok()?;
    let client_email = std::env::var("SENTRY_SYMBOLICATOR_GCS_CLIENT_EMAIL").ok()?;

    if private_key.is_empty() || client_email.is_empty() {
        None
    } else {
        Some(GcsSourceKey {
            private_key: GcsPrivateKey(private_key.into()),
            client_email,
        })
    }
}

/// Returns the legacy read-only GCS credentials for testing GCS support.
///
/// If the credentials are not available this will exit the test early, as a poor substitute
/// for skipping tests.
#[macro_export]
macro_rules! gcs_source_key {
    () => {
        match $crate::gcs_source_key_from_env() {
            Some(key) => key,
            None => {
                println!("Skipping due to missing SENTRY_SYMBOLICATOR_GCS_PRIVATE_KEY or SENTRY_SYMBOLICATOR_GCS_CLIENT_EMAIL");
                return;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGcsCredentials {
    pub bucket: String,
    pub credentials_file: Option<PathBuf>,
}

/// Return path to service account credentials.
///
/// Looks for a file named `gcs-service-account.json` in the git root which is expected to
/// contain GCP credentials to be used with `gcp_auth::from_credentials_file`.
///
/// If the environment variable `GOOGLE_APPLICATION_CREDENTIALS_JSON` exists it is written
/// into this file first, this is used to support secrets in our CI.
///
/// If the file is not found, returns `None`.
///
/// Sentry employees can find this file under the `symbolicator-gcs-test-key` entry in
/// 1Password.
pub fn gcs_credentials_file() -> Result<Option<PathBuf>, std::io::Error> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // to /crates/
    path.pop(); // to /
    path.push("gcs-service-account.json");

    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => {
                // Special case, see if we can create it from a env var, our CI sets this.
                match std::env::var("GOOGLE_APPLICATION_CREDENTIALS_JSON") {
                    Ok(data) if !data.is_empty() => {
                        std::fs::write(&path, data).unwrap();
                        path
                    }
                    _ => return Ok(None),
                }
            }
            _ => return Err(err),
        },
    };

    match path.exists() {
        true => Ok(Some(path)),
        false => Ok(None),
    }
}

/// Returns GCS credentials for testing GCS support.
///
/// This first uses [`gcs_credentials_file`] to find credentials, if not it checks if the
/// `GOOGLE_APPLICATION_CREDENTIALS` environment is set which is directly interpreted by the
/// `gcp_auth` crate.
///
/// Finally if nothing can be found this macro will return, as a poor substitute for
/// skipping tests.
#[macro_export]
macro_rules! gcs_credentials {
    () => {
        match $crate::gcs_credentials_file() {
            Ok(Some(path)) => {
                $crate::TestGcsCredentials {
                    bucket: "sentryio-symbolicator-cache-test".to_string(),
                    credentials_file: Some(path),
                }
            },
            Ok(None) => {
                match std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
                    Ok(path) => $crate::TestGcsCredentials {
                        bucket: "sentryio-symbolicator-cache-test".to_string(),
                        credentials_file: Some(path.into()),
                    },
                    Err(_) => {
                        println!("Skipping due to missing GOOGLE_APPLICATION_CREDENTIALS or gcs-service-account.json");
                        return;
                    }
                }
            }
            Err(err) => panic!("{}", err),
        }
    }
}

/// Helper to redact the port number from localhost URIs in insta snapshots.
///
/// Since we use a localhost source on a random port during tests we get random port
/// numbers in URI of the dif object file candidates.  This redaction masks this out.
pub fn redact_localhost_port(
    value: insta::internals::Content,
    _path: insta::internals::ContentPath<'_>,
) -> insta::internals::Content {
    value
        .as_str()
        .map(|value| {
            let re = ::regex::Regex::new(r"^http://localhost:[0-9]+").unwrap();
            re.replace(value, "http://localhost:<port>")
                .into_owned()
                .into()
        })
        .unwrap_or(value)
}

/// Helper to redact timestamps used for cache busting in insta snapshots.
pub fn redact_timestamp(
    value: insta::internals::Content,
    _path: insta::internals::ContentPath<'_>,
) -> insta::internals::Content {
    value
        .as_str()
        .map(|value| {
            let re = ::regex::Regex::new(r"#[0-9]+").unwrap();
            re.replace(value, "").into_owned().into()
        })
        .unwrap_or(value)
}

#[macro_export]
macro_rules! assert_snapshot {
    ($e:expr) => {
        ::insta::assert_yaml_snapshot!($e, {
            ".**.location" => ::insta::dynamic_redaction(
                $crate::redact_localhost_port
            ),
            ".**.abs_path" => ::insta::dynamic_redaction(
                $crate::redact_localhost_port
            ),
            ".**.data.sourcemap" => ::insta::dynamic_redaction(
                $crate::redact_localhost_port
            ),
            ".**.data.sourcemap_origin" => ::insta::dynamic_redaction(
                $crate::redact_localhost_port,
            ),
            ".**.data.sourcemap_origin" => ::insta::dynamic_redaction(
                $crate::redact_timestamp,
            ),
            ".**.scraping_attempts[].url" => ::insta::dynamic_redaction(
                $crate::redact_localhost_port
            ),
        });
    }
}
