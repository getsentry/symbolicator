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

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::{FutureExt, TryFutureExt};
use log::LevelFilter;
use reqwest::Url;
use warp::filters::fs::File;
use warp::reject::{Reject, Rejection};
use warp::Filter;

use crate::sources::{
    CommonSourceConfig, FileType, FilesystemSourceConfig, HttpSourceConfig, SourceConfig,
    SourceFilters, SourceId,
};

pub use actix_web::test::TestServer;
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

/// Runs the provided function, blocking the current thread until the result **legacy**
/// [`Future`](futures01::Future) completes.
///
/// This function can be used to synchronously block the current thread until the provided `Future`
/// has resolved either successfully or with an error. The result of the future is then returned
/// from this function call.
///
/// This is provided rather than a `block_on`-like interface to avoid accidentally calling a
/// function which spawns before creating a future, which would attempt to spawn before actix is
/// initialised.
///
/// Note that this function is intended to be used only for testing purpose. This function panics on
/// nested call.
pub async fn spawn_compat<F, T>(f: F) -> T::Output
where
    F: FnOnce() -> T + Send + 'static,
    T: Future + 'static,
    T::Output: Send,
{
    let (sender, receiver) = futures::channel::oneshot::channel();

    std::thread::spawn(|| {
        let result = tokio01::runtime::current_thread::Runtime::new()
            .unwrap()
            .block_on(f().never_error().boxed_local().compat());

        sender.send(result)
    });

    match receiver.await.unwrap() {
        Ok(output) => output,
        Err(never) => match never {},
    }
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

        let server = Server::new(reject.or(not_found).or(pending));

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
            files: files_config,
        }));

        FailingSymbolServer {
            server,
            times_accessed,
            reject_source,
            pending_source,
            not_found_source,
        }
    }

    pub(crate) fn accesses(&self) -> usize {
        self.times_accessed.swap(0, Ordering::SeqCst)
    }
}
// make sure procspawn works.
procspawn::enable_test_support!();
