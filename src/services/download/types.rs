//! Common types for download service.
//!
//! Mostly here to avoid circular imports.

use failure::Fail;

#[derive(Debug, Fail, Clone)]
pub enum DownloadErrorKind {
    #[fail(display = "failed to download")]
    Io,
    #[fail(display = "bad file destination")]
    BadDestination,
    #[fail(display = "failed writing the downloaded file")]
    Write,
    #[fail(display = "download was cancelled")]
    Canceled,
    #[fail(display = "temporary fudge error, not to be committed")]
    Tmp,
}

symbolic::common::derive_failure!(
    DownloadError,
    DownloadErrorKind,
    doc = "Errors happening while downloading from sources."
);

/// Completion status of a successful download request.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum DownloadStatus {
    /// The download completed successfully and the file at the path can be used.
    Completed,
    /// The requested file was not found, there is no useful data at the provided path.
    NotFound,
}

/// Common (transitional) type in all downloaders.
pub type DownloadStream =
    Box<dyn futures01::stream::Stream<Item = bytes::Bytes, Error = DownloadError>>;
