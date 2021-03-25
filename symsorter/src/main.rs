//! Sorts debug symbols into the right structure for symbolicator.

#![warn(missing_docs, missing_debug_implementations, clippy::all)]

#[macro_use]
mod utils;

mod app;
mod config;
mod difs;
mod plist;

fn main() {
    app::main();
}
