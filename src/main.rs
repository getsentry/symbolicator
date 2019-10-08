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

mod actors;
mod app;
mod cache;
mod config;
mod endpoints;
mod logging;
mod middlewares;
mod types;
mod utils;

#[cfg(test)]
mod test;

fn main() {
    self::app::main();
}
