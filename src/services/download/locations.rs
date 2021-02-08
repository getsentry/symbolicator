//! Types and implementations for dealing with object file locations
//!
//! This provides the [`ObjectFileSource`] type which provides a unified way of dealing with
//! an object file which may exist on a source.

use std::fmt;
use std::path::Path;

use anyhow::{Error, Result};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::sources::SourceId;
use crate::utils::sentry::ConfigureScope;

use super::filesystem::FilesystemObjectFileSource;
use super::gcs::GcsObjectFileSource;
use super::http::HttpObjectFileSource;
use super::s3::S3ObjectFileSource;
use super::sentry::SentryObjectFileSource;

/// A location for a file retrievable from many source configs.
///
/// It is essentially a `/`-separated string. This is currently used by all sources other than
/// [`SentrySourceConfig`]. This may change in the future.
///
/// [`SentrySourceConfig`]: crate::sources::SentrySourceConfig
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct SourceLocation(String);

impl SourceLocation {
    pub fn new(loc: impl Into<String>) -> Self {
        SourceLocation(loc.into())
    }

    /// Return an iterator of the location segments.
    pub fn segments<'a>(&'a self) -> impl Iterator<Item = &str> + 'a {
        self.0.split('/').filter(|s| !s.is_empty())
    }

    /// Returns this location as a local (relative) Path.
    pub fn path(&self) -> &Path {
        &Path::new(&self.0)
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
    pub fn to_url(&self, base: &Url) -> Result<Url> {
        let mut joined = base.clone();
        joined
            .path_segments_mut()
            .map_err(|_| Error::msg("URL cannot-be-a-base"))?
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

/// Represents a single object file stored on a source.
///
/// This joins the file location together with a [`SourceConfig`] and thus provides all
/// information to retrieve the object file from its source.
///
/// [`SourceConfig`]: crate::sources::SourceConfig
#[derive(Debug, Clone)]
pub enum ObjectFileSource {
    Sentry(SentryObjectFileSource),
    Http(HttpObjectFileSource),
    S3(S3ObjectFileSource),
    Gcs(GcsObjectFileSource),
    Filesystem(FilesystemObjectFileSource),
}

impl fmt::Display for ObjectFileSource {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ObjectFileSource::Sentry(ref s) => {
                write!(f, "Sentry source '{}' file id '{}'", s.source.id, s.file_id)
            }
            ObjectFileSource::Http(ref s) => {
                write!(f, "HTTP source '{}' location '{}'", s.source.id, s.location)
            }
            ObjectFileSource::S3(ref s) => {
                write!(f, "S3 source '{}' location '{}'", s.source.id, s.location)
            }
            ObjectFileSource::Gcs(ref s) => {
                write!(f, "GCS source '{}' location '{}'", s.source.id, s.location)
            }
            ObjectFileSource::Filesystem(ref s) => {
                write!(
                    f,
                    "Filesystem source '{}' location '{}'",
                    s.source.id, s.location
                )
            }
        }
    }
}

impl ObjectFileSource {
    /// Whether debug files from this source may be shared.
    pub fn is_public(&self) -> bool {
        match self {
            ObjectFileSource::Sentry(_) => false,
            ObjectFileSource::Http(ref x) => x.source.files.is_public,
            ObjectFileSource::S3(ref x) => x.source.files.is_public,
            ObjectFileSource::Gcs(ref x) => x.source.files.is_public,
            ObjectFileSource::Filesystem(ref x) => x.source.files.is_public,
        }
    }

    pub fn cache_key(&self) -> String {
        match self {
            ObjectFileSource::Sentry(ref x) => {
                format!("{}.{}.sentryinternal", x.source.id, x.file_id)
            }
            ObjectFileSource::Http(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            ObjectFileSource::S3(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            ObjectFileSource::Gcs(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            ObjectFileSource::Filesystem(ref x) => {
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
            ObjectFileSource::Sentry(ref x) => &x.source.id,
            ObjectFileSource::Http(ref x) => &x.source.id,
            ObjectFileSource::S3(ref x) => &x.source.id,
            ObjectFileSource::Gcs(ref x) => &x.source.id,
            ObjectFileSource::Filesystem(ref x) => &x.source.id,
        }
    }

    pub fn source_type_name(&self) -> &'static str {
        match *self {
            ObjectFileSource::Sentry(..) => "sentry",
            ObjectFileSource::S3(..) => "s3",
            ObjectFileSource::Gcs(..) => "gcs",
            ObjectFileSource::Http(..) => "http",
            ObjectFileSource::Filesystem(..) => "filesystem",
        }
    }

    /// Returns a URI for the location of the object file.
    ///
    /// There is no guarantee about any format of this URI, for some sources it could be
    /// very abstract.  In general the source should try and producde a URI which can be
    /// used directly into the source-specific tooling.  E.g. for an HTTP source this would
    /// be an `http://` or `https://` URL, for AWS S3 it would be an `s3://` url etc.
    pub fn uri(&self) -> ObjectFileSourceUri {
        match self {
            ObjectFileSource::Sentry(ref file_source) => file_source.uri(),
            ObjectFileSource::Http(ref file_source) => file_source.uri(),
            ObjectFileSource::S3(ref file_source) => file_source.uri(),
            ObjectFileSource::Gcs(ref file_source) => file_source.uri(),
            ObjectFileSource::Filesystem(ref file_source) => file_source.uri(),
        }
    }
}

impl ConfigureScope for ObjectFileSource {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        scope.set_tag("source.id", self.source_id());
        scope.set_tag("source.type", self.source_type_name());
        scope.set_tag("source.is_public", self.is_public());
    }
}

/// A URI representing an [`ObjectFileSource`].
///
/// Note that this does not provide enough information to download the object file, for this
/// you need the actual [`ObjectFileSource`].  The purpose of this URI is to be able to
/// display to a user who might be able to use this in other tools.  E.g. for an S3 source
/// this could be an `s3://` URI etc.
///
/// [`ObjectFileSource`]: crate::services::download::ObjectFileSource
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct ObjectFileSourceUri(String);

impl ObjectFileSourceUri {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl<T> From<T> for ObjectFileSourceUri
where
    T: Into<String>,
{
    fn from(source: T) -> Self {
        Self(source.into())
    }
}

impl fmt::Display for ObjectFileSourceUri {
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
