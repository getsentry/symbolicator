//! Various caching primitives for use in Symbolicator.
//!
//! These primitives can be composed and layered on top of each other.
//!
//! Currently there is a [`ComputationCache`] that provides request coalescing and
//! keeps cache entries in memory for some time.

#![warn(missing_docs)]

mod computation;
// mod fscache;

pub use computation::*;
// pub use fscache::*;

#[cfg(any(test, feature = "test"))]
pub(crate) use tokio::time;

#[cfg(not(any(test, feature = "test")))]
pub(crate) use std::time;
