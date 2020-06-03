//! Common tools for (web) clients of the download service.

/// HTTP User-Agent string to use.
pub const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));
