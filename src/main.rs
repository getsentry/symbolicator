//! Symbolicator.
//!
//! Symbolicator is a standalone web service that resolves function names, file location and source
//! context in native stack traces. It can process Minidumps and Apple Crash Reports. Additionally,
//! Symbolicator can act as a proxy to symbol servers supporting multiple formats, such as
//! Microsoft's symbol server or Breakpad symbol repositories.

#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![feature(async_closure)]

#[macro_use]
mod macros;

#[macro_use]
mod metrics;

mod actors;
mod app;
mod cache;
mod cli;
mod config;
mod endpoints;
mod logging;
mod middlewares;
mod server;
mod types;
mod utils;

#[cfg(test)]
mod test;

fn main() {
    match cli::execute() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            logging::ensure_log_error(&error);
            std::process::exit(1);
        }
    }
}
