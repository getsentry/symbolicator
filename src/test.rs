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
use std::path::PathBuf;
use std::sync::Arc;

use actix_web::fs::StaticFiles;
use futures::{FutureExt, TryFutureExt};
use log::LevelFilter;

use crate::sources::{
    CommonSourceConfig, FileType, FilesystemSourceConfig, HttpSourceConfig, SourceConfig,
    SourceFilters, SourceId,
};

pub use actix_web::test::*;
pub use tempfile::TempDir;

const SYMBOLS_PATH: &str = "tests/fixtures/symbols";

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
        path: PathBuf::from(SYMBOLS_PATH),
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

/// Spawn an actual HTTP symbol server for local fixtures.
///
/// The symbol server serves static files from the local symbols fixture location under the
/// `/download` prefix. The layout of this folder is [`DirectoryLayoutType::Native`]. This function
/// returns the test server as well as a source configuration, which can be used to access the
/// symbol server in symbolication requests.
///
/// **Note**: The symbol server runs on localhost. By default, connections to local host are not
/// permitted, and need to be activated via [`Config::connect_to_reserved_ips`].
pub(crate) fn symbol_server() -> (TestServer, SourceConfig) {
    let server = TestServer::new(|app| {
        app.resource("/download/{tail:.+}", |resource| {
            resource.h(StaticFiles::new(SYMBOLS_PATH).unwrap())
        });
    });

    // The source uses the same identifier ("local") as the local file system source to avoid
    // differences when changing the bucket in tests.

    let source = SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: SourceId::new("local"),
        url: server.url("/download/").parse().unwrap(),
        headers: Default::default(),
        files: Default::default(),
    }));

    (server, source)
}

// make sure procspawn works.
procspawn::enable_test_support!();
