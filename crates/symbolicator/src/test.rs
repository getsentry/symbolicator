//! Helpers for testing the web server and service.
//!
//! When writing tests, keep the following points in mind:
//!
//!  - In every test, call [`test::setup`]. This will set up the logger so that all console output
//!    is captured by the test runner.
//!
//!  - When using [`test::tempdir`], make sure that the handle to the temp directory is held for the
//!    entire lifetime of the test. When dropped too early, this might silently leak the temp
//!    directory, since symbolicator will create it again lazily after it has been deleted. To avoid
//!    this, assign it to a variable in the test function (e.g. `let _cache_dir = test::tempdir()`).
//!
//!  - When using [`test::symbol_server`], make sure that the server is held until all requests to
//!    the server have been made. If the server is dropped, the ports remain open and all
//!    connections to it will time out. To avoid this, assign it to a variable: `let (_server,
//!    source) = test::symbol_server();`. Alternatively, use [`test::local_source`] to test without
//!    HTTP connections.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use log::LevelFilter;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use warp::filters::fs::File;
use warp::reject::{Reject, Rejection};
use warp::Filter;

use crate::config::Config;
use crate::endpoints;
use crate::services::Service;
use crate::sources::{
    CommonSourceConfig, FileType, FilesystemSourceConfig, GcsSourceKey, HttpSourceConfig,
    SourceConfig, SourceFilters, SourceId,
};

pub use tempfile::TempDir;

/// Setup the test environment.
///
///  - Initializes logs: The logger only captures logs from the `symbolicator` crate and mutes all
///    other logs (such as actix or symbolic).
pub(crate) fn setup() {
    env_logger::builder()
        .filter(Some("symbolicator"), LevelFilter::Trace)
        .is_test(true)
        .try_init()
        .ok();
}

/// Create a default [`Service`] running with the current Runtime.
pub(crate) async fn default_service() -> Service {
    let handle = tokio::runtime::Handle::current();
    Service::create(Config::default(), handle.clone(), handle.clone())
        .await
        .unwrap()
}

/// Creates a temporary directory.
///
/// The directory is deleted when the [`TempDir`] instance is dropped, unless
/// [`into_path`](TemptDir::into_path) is called. Use it as a guard to automatically clean up after
/// tests.
pub(crate) fn tempdir() -> TempDir {
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
pub(crate) fn fixture(path: impl AsRef<Path>) -> PathBuf {
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
pub(crate) fn read_fixture(path: impl AsRef<Path>) -> Vec<u8> {
    std::fs::read(fixture(path)).unwrap()
}

/// Get bucket configuration for the local fixtures.
///
/// Files are served directly via the local file system without the indirection through a HTTP
/// symbol server. This is the fastest way for testing, but also avoids certain code paths.
pub(crate) fn local_source() -> SourceConfig {
    SourceConfig::Filesystem(Arc::new(FilesystemSourceConfig {
        id: SourceId::new("local"),
        path: fixture("symbols"),
        files: Default::default(),
    }))
}

/// Get bucket configuration for the microsoft symbol server.
#[allow(dead_code)]
pub(crate) fn microsoft_symsrv() -> SourceConfig {
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

/// Custom version of warp's sealed `IsReject` trait.
///
/// This is required to allow the test [`Server`] to spawn a warp server.
pub(crate) trait IsReject {}

impl IsReject for warp::reject::Rejection {}
impl IsReject for std::convert::Infallible {}

/// A test server that binds to a random port and serves a web app.
///
/// This server requires a `tokio` runtime and is supposed to be run in a `tokio::test`. It
/// automatically stops serving when dropped.
#[derive(Debug)]
pub(crate) struct Server {
    handle: tokio::task::JoinHandle<()>,
    socket: SocketAddr,
}

impl Server {
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

    pub fn with_service(service: Service) -> Self {
        let socket = SocketAddr::from(([127, 0, 0, 1], 0));

        let server =
            axum::Server::bind(&socket).serve(endpoints::create_app(service).into_make_service());

        let socket = server.local_addr();
        let handle = tokio::spawn(async {
            let _ = server.await;
        });

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

/// Spawn an actual HTTP symbol server for local fixtures.
///
/// The symbol server serves static files from the local symbols fixture location under the
/// `/download` prefix. The layout of this folder is [`DirectoryLayoutType::Native`]. This function
/// returns the test server as well as a source configuration, which can be used to access the
/// symbol server in symbolication requests.
///
/// **Note**: The symbol server runs on localhost. By default, connections to local host are not
/// permitted, and need to be activated via [`Config::connect_to_reserved_ips`].
pub(crate) fn symbol_server() -> (Server, SourceConfig) {
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

#[derive(Debug, Clone, Copy)]
struct GoAway;

impl Reject for GoAway {}

pub(crate) struct FailingSymbolServer {
    #[allow(unused)]
    server: Server,
    times_accessed: Arc<AtomicUsize>,
    pub(crate) reject_source: SourceConfig,
    pub(crate) pending_source: SourceConfig,
    pub(crate) not_found_source: SourceConfig,
    pub(crate) forbidden_source: SourceConfig,
}

impl FailingSymbolServer {
    pub(crate) fn new() -> Self {
        let times_accessed = Arc::new(AtomicUsize::new(0));

        let times = times_accessed.clone();
        let reject = warp::path("reject").and_then(move || {
            let times = Arc::clone(&times);
            async move {
                (*times).fetch_add(1, Ordering::SeqCst);

                Err::<File, _>(warp::reject::custom(GoAway))
            }
        });

        let times = times_accessed.clone();
        let not_found = warp::path("not-found").and_then(move || {
            let times = Arc::clone(&times);
            async move {
                (*times).fetch_add(1, Ordering::SeqCst);

                Err::<File, _>(warp::reject::not_found())
            }
        });

        let times = times_accessed.clone();
        let pending = warp::path("pending").and_then(move || {
            (*times).fetch_add(1, Ordering::SeqCst);

            std::future::pending::<Result<File, Rejection>>()
        });

        let times = times_accessed.clone();
        let forbidden = warp::path("forbidden").and_then(move || {
            (*times).fetch_add(1, Ordering::SeqCst);

            async move {
                let result: Result<_, Rejection> = Ok(warp::reply::with_status(
                    warp::reply(),
                    warp::http::StatusCode::FORBIDDEN,
                ));
                result
            }
        });

        let server = Server::new(reject.or(not_found).or(pending).or(forbidden));

        let files_config =
            CommonSourceConfig::with_layout(crate::sources::DirectoryLayoutType::Unified);

        let reject_source = SourceConfig::Http(Arc::new(HttpSourceConfig {
            id: SourceId::new("reject"),
            url: server.url("reject/"),
            headers: Default::default(),
            files: files_config.clone(),
        }));

        let pending_source = SourceConfig::Http(Arc::new(HttpSourceConfig {
            id: SourceId::new("pending"),
            url: server.url("pending/"),
            headers: Default::default(),
            files: files_config.clone(),
        }));

        let not_found_source = SourceConfig::Http(Arc::new(HttpSourceConfig {
            id: SourceId::new("not-found"),
            url: server.url("not-found/"),
            headers: Default::default(),
            files: files_config.clone(),
        }));

        let forbidden_source = SourceConfig::Http(Arc::new(HttpSourceConfig {
            id: SourceId::new("forbidden"),
            url: server.url("forbidden/"),
            headers: Default::default(),
            files: files_config,
        }));

        FailingSymbolServer {
            server,
            times_accessed,
            reject_source,
            pending_source,
            not_found_source,
            forbidden_source,
        }
    }

    pub(crate) fn accesses(&self) -> usize {
        self.times_accessed.swap(0, Ordering::SeqCst)
    }
}

// make sure procspawn works.
procspawn::enable_test_support!();

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
macro_rules! gcs_source_key {
    () => {
        match $crate::test::gcs_source_key_from_env() {
            Some(key) => key,
            None => {
                println!("Skipping due to missing SENTRY_SYMBOLICATOR_GCS_PRIVATE_KEY or SENTRY_SYMBOLICATOR_GCS_CLIENT_EMAIL");
                return;
            }
        }
    }
}

// Allow other modules to use this macro.
pub(crate) use gcs_source_key;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGcsCredentials {
    pub bucket: String,
    pub credentials_file: Option<PathBuf>,
}

/// Return path to service account credentials.
///
/// Looks for a file named `gcs-service-account.json` in the git root which is expected to
/// contain GCP credentials to be used with [`gcp_auth::from_credentials_file`].
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
                    Ok(data) => {
                        std::fs::write(&path, data).unwrap();
                        path
                    }
                    Err(_) => return Ok(None),
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
macro_rules! gcs_credentials {
    () => {
        match $crate::test::gcs_credentials_file() {
            Ok(Some(path)) => {
                $crate::test::TestGcsCredentials {
                    bucket: "sentryio-symbolicator-cache-test".to_string(),
                    credentials_file: Some(path),
                }
            },
            Ok(None) => {
                match std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
                    Ok(path) => $crate::test::TestGcsCredentials {
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

// Allow other modules to use this macro.
pub(crate) use gcs_credentials;
