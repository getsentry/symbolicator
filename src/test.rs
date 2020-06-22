//! Helpers for testing the web server and service.
//!
//! When writing tests, keep the following points in mind:
//!
//!  - In every test, call `test::setup`. This will set up the logger so that all console output is
//!    captured by the test runner.
//!
//!  - When using `test::tempdir`, make sure that the handle to the temp directory is held for the
//!    entire lifetime of the test. When dropped too early, this might silently leak the temp
//!    directory, since symbolicator will create it again lazily after it has been deleted. To avoid
//!    this, assign it to a variable in the test function (e.g. `let _cache_dir = test::tempdir()`).
//!
//!  - When using `test::symbol_server`, make sure that the server is held until all requests to the
//!    server have been made. If the server is dropped, the ports remain open and all connections
//!    to it will time out. To avoid this, assign it to a variable: `let (_server, source) =
//!    test::symbol_server();`. Alternatively, use `test::local_source()` to test without HTTP
//!    connections.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use actix::{System, SystemRunner};
use actix_web::fs::StaticFiles;
use futures01::{future, IntoFuture};
use log::LevelFilter;

use crate::sources::{FilesystemSourceConfig, HttpSourceConfig, SourceConfig};

pub use actix_web::test::*;
pub use tempfile::TempDir;

const SYMBOLS_PATH: &str = "tests/fixtures/symbols";

thread_local! {
    static SYSTEM: RefCell<Inner> = RefCell::new(Inner::new());
}

struct Inner {
    system: Option<System>,
    runner: Option<SystemRunner>,
}

impl Inner {
    fn new() -> Self {
        let runner = System::new("symbolicator_test");
        let system = System::current();

        Self {
            system: Some(system),
            runner: Some(runner),
        }
    }

    fn set_current(&self) {
        System::set_current(self.system.clone().unwrap());
    }

    fn runner(&mut self) -> &mut SystemRunner {
        self.runner.as_mut().unwrap()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.system.take();
        std::mem::forget(self.runner.take().unwrap())
    }
}

/// Setup the test environment.
///
///  - Initializes logs: The logger only captures logs from the `symbolicator` crate and mutes all
///    other logs (such as actix or symbolic).
///  - Switches threadpools into test mode: In this mode, threadpools do not actually spawn threads,
///    but instead return the futures that are spawned. This allows to capture console output logged
///    from spawned tasks.
pub(crate) fn setup() {
    env_logger::builder()
        .filter(Some("symbolicator"), LevelFilter::Trace)
        .is_test(true)
        .try_init()
        .ok();

    // Force initialization of the actix system
    SYSTEM.with(|_sys| ());

    crate::utils::futures::enable_test_mode();
}

/// Creates a temporary directory.
///
/// The directory is deleted when the `TempDir` instance is dropped, unless `into_path()` is called.
/// Use it as a guard to automatically clean up after tests.
pub(crate) fn tempdir() -> TempDir {
    TempDir::new().unwrap()
}

/// Runs the provided function, blocking the current thread until the result
/// future completes.
///
/// This function can be used to synchronously block the current thread
/// until the provided `future` has resolved either successfully or with an
/// error. The result of the future is then returned from this function
/// call.
///
/// Note that this function is intended to be used only for testing purpose.
/// This function panics on nested call.
pub fn block_fn<F, R>(f: F) -> Result<R::Item, R::Error>
where
    F: FnOnce() -> R,
    R: IntoFuture,
{
    SYSTEM.with(|cell| {
        let mut inner = cell.borrow_mut();
        inner.set_current();
        inner.runner().block_on(future::lazy(f))
    })
}

/// Run the std::future::Future until completion.
///
/// Block the current thread and run the std::future::Future aka futures03 future to
/// completion, returning the output it resolves to.
pub fn block_on<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    use futures::future::{FutureExt, TryFutureExt};

    SYSTEM.with(|cell| {
        let mut inner = cell.borrow_mut();
        inner.set_current();
        inner
            .runner()
            .block_on(f.then(futures::future::ok::<T, ()>).boxed_local().compat())
            .unwrap()
    })
}

/// Get bucket configuration for the local fixtures.
///
/// Files are served directly via the local file system without the indirection through a HTTP
/// symbol server. This is the fastest way for testing, but also avoids certain code paths.
#[allow(unused)]
pub(crate) fn local_source() -> SourceConfig {
    SourceConfig::Filesystem(Arc::new(FilesystemSourceConfig {
        id: "local".to_owned(),
        path: PathBuf::from(SYMBOLS_PATH),
        files: Default::default(),
    }))
}

/// Spawn an actual HTTP symbol server for local fixtures.
///
/// The symbol server serves static files from the local symbols fixture location under the
/// `/download` prefix. The layout of this folder is `Native`. This function returns the test server
/// as well as a source configuration, which can be used to access the symbol server in
/// symbolication requests.
///
/// **Note**: The symbol server runs on localhost. By default, connections to local host are not
/// permitted, and need to be activated via `Config::connect_to_reserved_ips`.
pub(crate) fn symbol_server() -> (TestServer, SourceConfig) {
    let server = TestServer::new(|app| {
        app.resource("/download/{tail:.+}", |resource| {
            resource.h(StaticFiles::new(SYMBOLS_PATH).unwrap())
        });
    });

    // The source uses the same identifier ("local") as the local file system source to avoid
    // differences when changing the bucket in tests.

    let source = SourceConfig::Http(Arc::new(HttpSourceConfig {
        id: "local".to_owned(),
        url: server.url("/download/").parse().unwrap(),
        headers: Default::default(),
        files: Default::default(),
    }));

    (server, source)
}

// make sure procspawn works.
procspawn::enable_test_support!();
