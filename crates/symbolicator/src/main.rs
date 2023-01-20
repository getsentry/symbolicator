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

pub use symbolicator_service::{config, metric, utils};

mod cli;
mod endpoints;
mod logging;
mod server;
mod service;

#[cfg(test)]
mod test {
    use crate::config::Config;
    use crate::service::RequestService;
    pub use symbolicator_test::*;

    use crate::endpoints;

    pub fn server_with_default_service() -> Server {
        let handle = tokio::runtime::Handle::current();
        let config = Config {
            connect_to_reserved_ips: true,
            ..Config::default()
        };
        let service = RequestService::create(config, handle.clone(), handle).unwrap();

        Server::with_router(endpoints::create_app(service))
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
