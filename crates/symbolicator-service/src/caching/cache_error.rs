use std::borrow::Cow;
use std::io;
use std::time::Duration;

use humantime_serde::re::humantime::{format_duration, parse_duration};
use symbolic::common::ByteView;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// An error that happens when fetching an object from a remote location.
///
/// This error enum is intended for persisting in caches, except for the
/// [`InternalError`](Self::InternalError) variant.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CacheError {
    /// The object was not found at the remote source.
    #[error("not found")]
    NotFound,
    /// The object could not be fetched from the remote source due to missing
    /// permissions.
    ///
    /// The attached string contains the remote source's response.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The object could not be fetched from the remote source due to a timeout.
    #[error("download timed out after {0:?}")]
    Timeout(Duration),
    /// The object could not be fetched from the remote source due to another problem,
    /// like connection loss, DNS resolution, or a 5xx server response.
    ///
    /// The attached string contains the remote source's response.
    #[error("download failed: {0}")]
    DownloadError(String),
    /// The object was fetched successfully, but is invalid in some way.
    ///
    /// For example, this could result from an unsupported object file or an error
    /// during symcache conversion
    #[error("malformed: {0}")]
    Malformed(String),
    /// The object is of a type that cannot be used for the symbolication task it was
    /// requested for.
    ///
    /// This is currently only used when we try to symbolicate a .NET event with a Windows
    /// PDB file. A tracking issue in `symbolic` for supporting this case is
    /// [here](https://github.com/getsentry/symbolic/issues/871).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// An unexpected error in symbolicator itself.
    ///
    /// This variant is not intended to be persisted to or read from caches.
    #[error("internal error")]
    InternalError,
}

impl From<std::io::Error> for CacheError {
    #[track_caller]
    fn from(err: std::io::Error) -> Self {
        Self::from_std_error(err)
    }
}

impl From<serde_json::Error> for CacheError {
    #[track_caller]
    fn from(err: serde_json::Error) -> Self {
        Self::from_std_error(err)
    }
}

impl CacheError {
    pub(super) const MALFORMED_MARKER: &'static [u8] = b"malformed";
    pub(super) const PERMISSION_DENIED_MARKER: &'static [u8] = b"permissiondenied";
    pub(super) const TIMEOUT_MARKER: &'static [u8] = b"timeout";
    pub(super) const DOWNLOAD_ERROR_MARKER: &'static [u8] = b"downloaderror";
    pub(super) const UNSUPPORTED_MARKER: &'static [u8] = b"unsupported";

    /// Writes error markers and details to a file.
    ///
    /// * If `self` is [`InternalError`](Self::InternalError), it does nothing.
    /// * If `self` is [`NotFound`](Self::NotFound), it empties the file.
    /// * In all other cases, it writes the corresponding marker, followed by the error
    ///   details, and truncates the file.
    pub async fn write(&self, file: &mut File) -> Result<(), io::Error> {
        if let Self::InternalError = self {
            tracing::error!("A `CacheError::InternalError` should never be written out");
            return Ok(());
        }
        file.rewind().await?;

        match self {
            CacheError::NotFound => {
                // NOOP
            }
            CacheError::Malformed(details) => {
                file.write_all(Self::MALFORMED_MARKER).await?;
                file.write_all(details.as_bytes()).await?;
            }
            CacheError::PermissionDenied(details) => {
                file.write_all(Self::PERMISSION_DENIED_MARKER).await?;
                file.write_all(details.as_bytes()).await?;
            }
            CacheError::Timeout(duration) => {
                file.write_all(Self::TIMEOUT_MARKER).await?;
                file.write_all(format_duration(*duration).to_string().as_bytes())
                    .await?;
            }
            CacheError::DownloadError(details) => {
                file.write_all(Self::DOWNLOAD_ERROR_MARKER).await?;
                file.write_all(details.as_bytes()).await?;
            }
            CacheError::Unsupported(details) => {
                file.write_all(Self::UNSUPPORTED_MARKER).await?;
                file.write_all(details.as_bytes()).await?;
            }
            CacheError::InternalError => {
                unreachable!("this was already handled above");
            }
        }

        let new_len = file.stream_position().await?;
        file.set_len(new_len).await?;

        Ok(())
    }

    /// Parses a `CacheError` from a byte slice.
    ///
    /// * If the slice starts with an error marker, the corresponding error variant will be returned.
    /// * If the slice is empty, [`NotFound`](Self::NotFound) will be returned.
    /// * Otherwise `None` is returned.
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if let Some(raw_message) = bytes.strip_prefix(Self::PERMISSION_DENIED_MARKER) {
            let err_msg = utf8_message(raw_message);
            Some(Self::PermissionDenied(err_msg.into_owned()))
        } else if let Some(raw_duration) = bytes.strip_prefix(Self::TIMEOUT_MARKER) {
            let raw_duration = utf8_message(raw_duration);
            match parse_duration(&raw_duration) {
                Ok(duration) => Some(Self::Timeout(duration)),
                Err(e) => {
                    tracing::error!(error = %e, "Failed to read timeout duration");
                    Some(Self::InternalError)
                }
            }
        } else if let Some(raw_message) = bytes.strip_prefix(Self::DOWNLOAD_ERROR_MARKER) {
            let err_msg = utf8_message(raw_message);
            Some(Self::DownloadError(err_msg.into_owned()))
        } else if let Some(raw_message) = bytes.strip_prefix(Self::MALFORMED_MARKER) {
            let err_msg = utf8_message(raw_message);
            Some(Self::Malformed(err_msg.into_owned()))
        } else if let Some(raw_message) = bytes.strip_prefix(Self::UNSUPPORTED_MARKER) {
            let err_msg = utf8_message(raw_message);
            Some(Self::Unsupported(err_msg.into_owned()))
        } else if bytes.is_empty() {
            Some(Self::NotFound)
        } else {
            None
        }
    }

    #[track_caller]
    pub fn from_std_error<E: std::error::Error + 'static>(e: E) -> Self {
        let dynerr: &dyn std::error::Error = &e; // tracing expects a `&dyn Error`
        tracing::error!(error = dynerr);
        Self::InternalError
    }
}

/// A "safer" [`String::from_utf8_lossy`].
///
/// This reads the string only up to the first NUL-byte.
/// We have observed broken cache files which were not properly truncated.
/// They had a valid `CacheError` prefix, followed by gigabytes worth of NUL-bytes, and some junk at the end.
fn utf8_message(raw_message: &[u8]) -> Cow<'_, str> {
    let raw_message = raw_message
        .split(|c| *c == b'\0')
        .next()
        .unwrap_or(raw_message);
    String::from_utf8_lossy(raw_message)
}

/// An entry in a cache, containing either `Ok(T)` or an error denoting the reason why an
/// object could not be fetched or is otherwise unusable.
pub type CacheEntry<T = ()> = Result<T, CacheError>;

/// Parses a [`CacheEntry`] from a [`ByteView`].
pub fn cache_entry_from_bytes(bytes: ByteView<'static>) -> CacheEntry<ByteView<'static>> {
    CacheError::from_bytes(&bytes).map(Err).unwrap_or(Ok(bytes))
}
