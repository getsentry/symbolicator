//! Types and implementations for dealing with remote file locations.
//!
//! This provides the [`RemoteFile`] type which provides a unified way of dealing with a file
//! which may exist on a source.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    FilesystemRemoteFile, GcsRemoteFile, HttpRemoteFile, S3RemoteFile, SentryRemoteFile, SourceId,
};

/// A location for a file retrievable from many source configs.
///
/// It is essentially a `/`-separated string. This is currently used by all sources other than
/// [`SentrySourceConfig`](crate::SentrySourceConfig). This may change in the future.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct SourceLocation(String);

impl SourceLocation {
    /// Creates a new [`SourceLocation`].
    pub fn new(loc: impl Into<String>) -> Self {
        SourceLocation(loc.into())
    }

    /// Return an iterator of the location segments.
    pub fn segments(&self) -> impl Iterator<Item = &str> + '_ {
        self.0.split('/').filter(|s| !s.is_empty())
    }

    /// Returns this location as a local (relative) Path.
    pub fn path(&self) -> &Path {
        Path::new(&self.0)
    }

    /// Returns this location relative to the given base.
    ///
    /// The base may be a filesystem path or a URI prefix. It is always assumed that the base is a
    /// directory.
    pub fn prefix(&self, prefix: &str) -> String {
        let trimmed = prefix.trim_matches(&['/'][..]);
        if trimmed.is_empty() {
            self.0.clone()
        } else {
            format!("{}/{}", trimmed, self.0)
        }
    }

    /// Returns this location joined to the given base URL.
    ///
    /// As opposed to [`Url::join`], this only supports relative paths. Each segment of the path is
    /// percent-encoded. Empty segments are skipped, for example, `foo//bar` is collapsed to
    /// `foo/bar`.
    ///
    /// The base URL is treated as directory. If it does not end with a slash, then a slash is
    /// automatically appended.
    ///
    /// Returns `Err` if the URL is cannot-be-a-base.
    pub fn to_url(&self, base: &Url) -> anyhow::Result<Url> {
        let mut joined = base.clone();
        joined
            .path_segments_mut()
            .map_err(|_| anyhow::Error::msg("URL cannot-be-a-base"))?
            .pop_if_empty()
            .extend(self.segments());
        Ok(joined)
    }
}

impl fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Represents a single Debug Information File stored on a source.
///
/// This joins the file location together with a [`SourceConfig`](crate::SourceConfig) and thus
/// provides all information to retrieve the DIF from its source.  The file could be any DIF type:
/// an auxiliary DIF or an object file.
#[derive(Debug, Clone)]
pub enum RemoteFile {
    /// A file on a filesystem source.
    Filesystem(FilesystemRemoteFile),
    /// A file on a gcs source.
    Gcs(GcsRemoteFile),
    /// A file on a http source.
    Http(HttpRemoteFile),
    /// A file on a S3 source.
    S3(S3RemoteFile),
    /// A file on a Sentry source.
    Sentry(SentryRemoteFile),
}

impl fmt::Display for RemoteFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Sentry(ref s) => {
                write!(f, "Sentry source '{}' file id '{}'", s.source.id, s.file_id)
            }
            Self::Http(ref s) => {
                write!(f, "HTTP source '{}' location '{}'", s.source.id, s.location)
            }
            Self::S3(ref s) => {
                write!(f, "S3 source '{}' location '{}'", s.source.id, s.location)
            }
            Self::Gcs(ref s) => {
                write!(f, "GCS source '{}' location '{}'", s.source.id, s.location)
            }
            Self::Filesystem(ref s) => {
                write!(
                    f,
                    "Filesystem source '{}' location '{}'",
                    s.source.id, s.location
                )
            }
        }
    }
}

impl RemoteFile {
    /// Whether files from this source may be shared.
    pub fn is_public(&self) -> bool {
        match self {
            Self::Sentry(_) => false,
            Self::Http(ref x) => x.source.files.is_public,
            Self::S3(ref x) => x.source.files.is_public,
            Self::Gcs(ref x) => x.source.files.is_public,
            Self::Filesystem(ref x) => x.source.files.is_public,
        }
    }

    /// A specific cache key for this [`RemoteFile`].
    pub fn cache_key(&self) -> String {
        match self {
            Self::Sentry(ref x) => {
                format!("{}.{}.sentryinternal", x.source.id, x.file_id)
            }
            Self::Http(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            Self::S3(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            Self::Gcs(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            Self::Filesystem(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
        }
    }

    /// Returns the ID of the source.
    ///
    /// Within one request these IDs should be unique, this includes any sources from the
    /// configuration which are available to all requests.
    pub fn source_id(&self) -> &SourceId {
        match self {
            Self::Sentry(ref x) => &x.source.id,
            Self::Http(ref x) => &x.source.id,
            Self::S3(ref x) => &x.source.id,
            Self::Gcs(ref x) => &x.source.id,
            Self::Filesystem(ref x) => &x.source.id,
        }
    }

    /// A short name for the source type.
    pub fn source_type_name(&self) -> &'static str {
        match *self {
            Self::Sentry(..) => "sentry",
            Self::S3(..) => "s3",
            Self::Gcs(..) => "gcs",
            Self::Http(..) => "http",
            Self::Filesystem(..) => "filesystem",
        }
    }

    /// Returns a key that uniquely identifies the source for metrics.
    ///
    /// If this is a built-in source the source_id is returned, otherwise this falls
    /// back to the source type name.
    pub fn source_metric_key(&self) -> &str {
        let id = self.source_id().as_str();
        // The IDs of built-in sources (see: SENTRY_BUILTIN_SOURCES in sentry) always start with
        // "sentry:" (e.g. "sentry:electron") and are safe to use as a key. If this is a custom
        // source, then the source_id is a random string which inflates the cardinality of this
        // metric as the tag values will greatly vary.
        if id.starts_with("sentry:") {
            id
        } else {
            self.source_type_name()
        }
    }

    /// Returns a URI for the location of the object file.
    ///
    /// There is no guarantee about any format of this URI, for some sources it could be
    /// very abstract.  In general the source should try and produce a URI which can be
    /// used directly into the source-specific tooling.  E.g. for an HTTP source this would
    /// be an `http://` or `https://` URL, for AWS S3 it would be an `s3://` url etc.
    pub fn uri(&self) -> RemoteFileUri {
        match self {
            Self::Sentry(file_source) => file_source.uri(),
            Self::Http(file_source) => file_source.uri(),
            Self::S3(file_source) => file_source.uri(),
            Self::Gcs(file_source) => file_source.uri(),
            Self::Filesystem(file_source) => file_source.uri(),
        }
    }
}

/// A URI representing an [`RemoteFile`].
///
/// Note that this does not provide enough information to download the object file, for this
/// you need the actual [`RemoteFile`].  The purpose of this URI is to be able to display to
/// a user who might be able to use this in other tools.  E.g. for an S3 source this could
/// be an `s3://` URI etc.
///
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct RemoteFileUri(String);

impl RemoteFileUri {
    /// Creates a new [`RemoteFileUri`].
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Constructs a new [`RemoteFileUri`] from parts.
    ///
    /// This percent-encodes the `path`.
    ///
    /// # Examples
    /// ```
    /// use symbolicator_sources::RemoteFileUri;
    ///
    /// let s3_uri = RemoteFileUri::from_parts("s3", "bucket", "path");
    /// assert_eq!(s3_uri, RemoteFileUri::new("s3://bucket/path"));
    ///
    /// let gcs_uri = RemoteFileUri::from_parts("gs", "bucket", "path with/spaces");
    /// assert_eq!(gcs_uri, RemoteFileUri::new("gs://bucket/path%20with/spaces"));
    /// ```
    pub fn from_parts(scheme: &str, host: &str, path: &str) -> Self {
        Url::parse(&format!("{scheme}://{host}/"))
            .and_then(|base| base.join(path))
            .map(RemoteFileUri::new)
            .unwrap_or_else(|_| {
                // All these Result-returning operations *should* be infallible and this
                // branch should never be used.  Nevertheless, for panic-safety we default
                // to something infallible that's also pretty correct.
                RemoteFileUri::new(format!("{scheme}://{host}/{path}"))
            })
    }
}

impl<T> From<T> for RemoteFileUri
where
    T: Into<String>,
{
    fn from(source: T) -> Self {
        Self(source.into())
    }
}

impl fmt::Display for RemoteFileUri {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_location_prefix() {
        let key = SourceLocation::new("spam/ham").prefix("");
        assert_eq!(key, "spam/ham");

        let key = SourceLocation::new("spam/ham").prefix("eggs");
        assert_eq!(key, "eggs/spam/ham");

        let key = SourceLocation::new("spam").prefix("/eggs/bacon/");
        assert_eq!(key, "eggs/bacon/spam");

        let key = SourceLocation::new("spam").prefix("//eggs//");
        assert_eq!(key, "eggs/spam");
    }

    #[test]
    fn test_location_url_empty() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = SourceLocation::new("").to_url(&base).unwrap();
        assert_eq!(joined, "https://example.org/base".parse().unwrap());
    }

    #[test]
    fn test_location_url_space() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = SourceLocation::new("foo bar").to_url(&base).unwrap();
        assert_eq!(
            joined,
            "https://example.org/base/foo%20bar".parse().unwrap()
        );
    }

    #[test]
    fn test_location_url_multiple() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = SourceLocation::new("foo/bar").to_url(&base).unwrap();
        assert_eq!(joined, "https://example.org/base/foo/bar".parse().unwrap());
    }

    #[test]
    fn test_location_url_trailing_slash() {
        let base = Url::parse("https://example.org/base/").unwrap();
        let joined = SourceLocation::new("foo").to_url(&base).unwrap();
        assert_eq!(joined, "https://example.org/base/foo".parse().unwrap());
    }

    #[test]
    fn test_location_url_leading_slash() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = SourceLocation::new("/foo").to_url(&base).unwrap();
        assert_eq!(joined, "https://example.org/base/foo".parse().unwrap());
    }

    #[test]
    fn test_location_url_multi_slash() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = SourceLocation::new("foo//bar").to_url(&base).unwrap();
        assert_eq!(joined, "https://example.org/base/foo/bar".parse().unwrap());
    }

    #[test]
    fn test_location_url_absolute() {
        let base = Url::parse("https://example.org/").unwrap();
        let joined = SourceLocation::new("foo").to_url(&base).unwrap();
        assert_eq!(joined, "https://example.org/foo".parse().unwrap());
    }
}
