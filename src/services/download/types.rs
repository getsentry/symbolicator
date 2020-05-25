//! Common types for download service.
//!
//! Mostly here to avoid circular imports.

use std::fmt;
use std::sync::Arc;

use failure::Fail;

use crate::types::{
    FilesystemSourceConfig, GcsSourceConfig, HttpSourceConfig, S3SourceConfig, SentrySourceConfig,
};

#[derive(Debug, Fail, Clone)]
pub enum DownloadErrorKind {
    #[fail(display = "failed to download")]
    Io,
    #[fail(display = "generic download failure, reason omitted/swallowed")]
    GenericFailed,
    #[fail(display = "bad file destination")]
    BadDestination,
    #[fail(display = "failed to create temporary download file")]
    TempFile,
    #[fail(display = "failed writing the downloaded file")]
    Write,
    #[fail(display = "temporary fudge error, not to be commited")]
    Tmp,
}

symbolic::common::derive_failure!(
    DownloadError,
    DownloadErrorKind,
    doc = "Errors happening while downloading from sources"
);

// impl From<failure::Context<DownloadError>> for DownloadError {
//     fn from(source: failure::Context<DownloadError>) -> Self {
//         source.get_context()
//     }
// }

/// A [SourceId] uniquely identifies a file on a source and how to download it.
///
/// This is a combination of the [SourceConfig], which describes a
/// download source and how to download from it, with an identifier
/// describing a single file in that source.
#[derive(Debug, Clone)]
pub enum SourceId {
    Sentry(Arc<SentrySourceConfig>, SentryFileId),
    S3(Arc<S3SourceConfig>, SourceLocation),
    Gcs(Arc<GcsSourceConfig>, SourceLocation),
    Http(Arc<HttpSourceConfig>, SourceLocation),
    Filesystem(Arc<FilesystemSourceConfig>, SourceLocation),
}

/// An identifier for a file retrievable from a [SentrySourceConfig].
#[derive(Debug, Clone)]
pub struct SentryFileId(String);

/// A location for a file retrievable from many source configs.
///
/// It is essentially a `/`-separated string.  This is currently used by all
/// sources other than [SentrySourceConfig].  This may change in the future.
#[derive(Debug, Clone)]
pub struct SourceLocation(String);

impl SourceLocation {
    /// Return an iterator of the location segments.
    pub fn segments<'a>(&'a self) -> impl std::iter::Iterator<Item = &str> + 'a {
        self.0.split('/').filter(|s| !s.is_empty())
    }
}

impl fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl SourceLocation {
    pub fn new(loc: impl Into<String>) -> Self {
        SourceLocation(loc.into())
    }
}
