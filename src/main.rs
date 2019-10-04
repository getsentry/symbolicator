//! Symbolicator.
//!
//! Symbolicator is a standalone web service that resolves function names, file location and source
//! context in native stack traces. It can process Minidumps and Apple Crash Reports. Additionally,
//! Symbolicator can act as a proxy to symbol servers supporting multiple formats, such as
//! Microsoft's symbol server or Breakpad symbol repositories.

#![warn(missing_docs)]
#![warn(missing_debug_implementations)]

#[macro_use]
mod macros;

#[macro_use]
mod metrics;

#[macro_use]
mod sentry;

mod actors;
mod app;
mod cache;
mod config;
mod endpoints;
mod hex;
mod http;
mod logging;
mod middlewares;
mod types;
mod utils;

fn main() {
    self::app::main();
}
