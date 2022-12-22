use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{CommonSourceConfig, RemoteFile, RemoteFileUri, SourceId, SourceLocation};

/// Configuration for reading from the local file system.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FilesystemSourceConfig {
    /// Unique source identifier.
    pub id: SourceId,

    /// Path to symbol directory.
    pub path: PathBuf,

    /// Configuration common to all sources.
    #[serde(flatten)]
    pub files: CommonSourceConfig,
}

/// Filesystem-specific [`RemoteDif`].
#[derive(Debug, Clone)]
pub struct FilesystemRemoteFile {
    pub(crate) source: Arc<FilesystemSourceConfig>,
    pub(crate) location: SourceLocation,
}

impl From<FilesystemRemoteFile> for RemoteFile {
    fn from(source: FilesystemRemoteFile) -> Self {
        Self::Filesystem(source)
    }
}

impl FilesystemRemoteFile {
    /// Creates a new [`FilesystemRemoteFile`].
    pub fn new(source: Arc<FilesystemSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the path from which to fetch this object file.
    pub fn path(&self) -> PathBuf {
        self.source.path.join(self.location.path())
    }

    /// Returns the `file://` URI from which to fetch this object file.
    ///
    /// This is a quick-and-dirty approximation, not fully RFC8089-compliant.  E.g. we do
    /// not provide a hostname nor percent-encode.  Use this only for diagnostics and use
    /// [`FilesystemRemoteFile::path`] if the actual file location is needed.
    pub(crate) fn uri(&self) -> RemoteFileUri {
        format!("file:///{}", self.path().display()).into()
    }
}
