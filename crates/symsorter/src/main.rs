//! Sorts debug symbols into the right structure for symbolicator.

#![warn(
    missing_docs,
    missing_debug_implementations,
    unused_crate_dependencies,
    clippy::all
)]

#[macro_use]
mod utils;

mod app;
mod config;

fn main() {
    app::main();
}
