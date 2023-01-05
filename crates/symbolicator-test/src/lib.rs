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

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract;
use axum::routing::get;
use axum::{middleware, Router};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::fmt;
use warp::Filter;

use symbolicator_sources::{
    CommonSourceConfig, DirectoryLayout, DirectoryLayoutType, FileType, FilesystemSourceConfig,
    GcsSourceKey, HttpSourceConfig, SourceConfig, SourceFilters, SourceId,
};

pub use tempfile::TempDir;

/// Setup the test environment.
///
///  - Initializes logs: The logger only captures logs from the `symbolicator` crate and mutes all
///    other logs (such as actix or symbolic).
pub fn setup() {
    fmt()
        .with_env_filter(EnvFilter::new("symbolicator-service=trace"))
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

/// Custom version of warp's sealed `IsReject` trait.
///
/// This is required to allow the test [`Server`] to spawn a warp server.
pub trait IsReject {}

impl IsReject for warp::reject::Rejection {}
impl IsReject for std::convert::Infallible {}

/// A test server that binds to a random port and serves a web app.
///
/// This server requires a `tokio` runtime and is supposed to be run in a `tokio::test`. It
/// automatically stops serving when dropped.
#[derive(Debug)]
pub struct Server {
    pub handle: tokio::task::JoinHandle<()>,
    pub socket: SocketAddr,
}

impl Server {
    fn with_router(router: Router) -> Self {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));

        let server = axum::Server::bind(&addr).serve(router.into_make_service());
        let socket = server.local_addr();

        let handle = tokio::spawn(async move {
            server.await.unwrap();
        });

        Self { handle, socket }
    }

    /// Creates a new test server from the given `warp` filter.
    pub fn new<F>(filter: F) -> Self
    where
        F: warp::Filter + Clone + Send + Sync + 'static,
        F::Extract: warp::reply::Reply,
        F::Error: IsReject,
    {
        let (socket, future) = warp::serve(filter).bind_ephemeral(([127, 0, 0, 1], 0));
        let handle = tokio::spawn(future);

        Self { handle, socket }
    }

    /// Returns the socket address that this server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.socket
    }

    /// Returns the port that this server listens on.
    pub fn port(&self) -> u16 {
        self.addr().port()
    }

    /// Returns a full URL pointing to the given path.
    ///
    /// This URL uses `localhost` as hostname.
    pub fn url(&self, path: &str) -> Url {
        let path = path.trim_start_matches('/');
        format!("http://localhost:{}/{}", self.port(), path)
            .parse()
            .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub struct HitCounter {
    server: Server,
    hits: Arc<Mutex<BTreeMap<String, usize>>>,
}

impl HitCounter {
    pub fn new() -> Self {
        let hits = Arc::new(Mutex::new(BTreeMap::new()));

        let hitcounter = {
            let hits = hits.clone();
            move |extract::OriginalUri(uri), req, next: middleware::Next<_>| {
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

        let router = Router::new()
            .route(
                "/redirect/*path",
                get(|extract::Path(path): extract::Path<String>| async move {
                    (StatusCode::FOUND, [("Location", format!("/{}", path))])
                }),
            )
            .route(
                "/delay/:time/*path",
                get(
                    |extract::Path((time, path)): extract::Path<(String, String)>| async move {
                        let duration = humantime::parse_duration(&time).unwrap();
                        tokio::time::sleep(duration).await;

                        (StatusCode::FOUND, [("Location", format!("/{}", path))])
                    },
                ),
            )
            .route(
                "/msdl/*path",
                get(|extract::Path(path): extract::Path<String>| async move {
                    let url = format!("https://msdl.microsoft.com/download/symbols/{}", path);
                    (StatusCode::FOUND, [("Location", url)])
                }),
            )
            .route(
                "/respond_statuscode/:num/*tail",
                get(
                    |extract::Path((num, _)): extract::Path<(u16, String)>| async move {
                        StatusCode::from_u16(num).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                    },
                ),
            )
            .route(
                "/garbage_data/*tail",
                get(|extract::Path(tail): extract::Path<String>| async move { tail }),
            )
            .layer(middleware::from_fn(hitcounter));

        let server = Server::with_router(router);

        Self { server, hits }
    }

    pub fn accesses(&self) -> usize {
        let map = std::mem::take(&mut *self.hits.lock().unwrap());
        map.into_values().sum()
    }

    pub fn all_hits(&self) -> Vec<(String, usize)> {
        let map = std::mem::take(&mut *self.hits.lock().unwrap());
        map.into_iter().collect()
    }

    pub fn url(&self, path: &str) -> Url {
        self.server.url(path)
    }

    pub fn source(&self, id: &str, path: &str) -> SourceConfig {
        let files = CommonSourceConfig {
            filters: SourceFilters {
                filetypes: vec![FileType::MachCode],
                path_patterns: vec![],
            },
            layout: Default::default(),
            is_public: false,
        };
        self.source_with_config(id, path, files)
    }

    pub fn source_with_config(
        &self,
        id: &str,
        path: &str,
        files: CommonSourceConfig,
    ) -> SourceConfig {
        SourceConfig::Http(Arc::new(HttpSourceConfig {
            id: SourceId::new(id),
            url: self.server.url(path),
            headers: Default::default(),
            files,
        }))
    }
}

impl Default for HitCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn an actual HTTP symbol server for local fixtures.
///
/// The symbol server serves static files from the local symbols fixture location under the
/// `/download` prefix. The layout of this folder is `DirectoryLayoutType::Native`. This function
/// returns the test server as well as a source configuration, which can be used to access the
/// symbol server in symbolication requests.
///
/// **Note**: The symbol server runs on localhost. By default, connections to local host are not
/// permitted, and need to be activated via `Config::connect_to_reserved_ips`.
pub fn symbol_server() -> (Server, SourceConfig) {
    let app = warp::path("download").and(warp::fs::dir(fixture("symbols")));
    let server = Server::new(app);

    // The source uses the same identifier ("local") as the local file system source to avoid
    // differences when changing the bucket in tests.

    let source = SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("local"),
        url: server.url("download/"),
        headers: Default::default(),
        files: Default::default(),
    }));

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
            private_key,
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
