//! Symbolicator.
//!
//! Symbolicator is a standalone web service that resolves function names, file location and source
//! context in native stack traces. It can process Minidumps and Apple Crash Reports. Additionally,
//! Symbolicator can act as a proxy to symbol servers supporting multiple formats, such as
//! Microsoft's symbol server or Breakpad symbol repositories.

#![warn(
    missing_docs,
    missing_debug_implementations,
    unused_crate_dependencies,
    clippy::all
)]

#[cfg(not(target_env = "msvc"))]
use jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

pub use symbolicator_service::*;

mod cli;
mod endpoints;
mod logging;
mod server;

#[cfg(test)]
mod test {
    use std::net::SocketAddr;

    use crate::config::Config;
    pub use symbolicator_test::*;

    use crate::endpoints;
    use crate::services::Service;

    pub async fn server_with_default_service() -> Server {
        let handle = tokio::runtime::Handle::current();
        let service = Service::create(Config::default(), handle.clone(), handle.clone())
            .await
            .unwrap();

        let socket = SocketAddr::from(([127, 0, 0, 1], 0));

        let server =
            axum::Server::bind(&socket).serve(endpoints::create_app(service).into_make_service());

        let socket = server.local_addr();
        let handle = tokio::spawn(async {
            let _ = server.await;
        });

        Server { handle, socket }
    }
}

fn main() {
    match cli::execute() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            logging::ensure_log_error(&error);
            std::process::exit(1);
        }
    }
}
