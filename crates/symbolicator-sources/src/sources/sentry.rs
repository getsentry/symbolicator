use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{RemoteFile, RemoteFileUri, SourceId};

/// Configuration for the Sentry-internal debug files endpoint.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SentrySourceConfig {
    /// Unique source identifier.
    pub id: SourceId,

    /// Absolute URL of the endpoint.
    pub url: Url,

    /// Bearer authorization token.
    pub token: String,
}

/// The Sentry-specific [`RemoteFile`].
#[derive(Debug, Clone)]
pub struct SentryRemoteFile {
    /// The underlying [`SentrySourceConfig`].
    pub source: Arc<SentrySourceConfig>,
    pub(crate) file_id: SentryFileId,
    pub(crate) r#type: SentryFileType,
}

impl From<SentryRemoteFile> for RemoteFile {
    fn from(source: SentryRemoteFile) -> Self {
        Self::Sentry(source)
    }
}

impl SentryRemoteFile {
    /// Creates a new [`SentryRemoteFile`].
    pub fn new(
        source: Arc<SentrySourceConfig>,
        file_id: SentryFileId,
        r#type: SentryFileType,
    ) -> Self {
        Self {
            source,
            file_id,
            r#type,
        }
    }

    /// Gives a synthetic [`RemoteFileUri`] for this file.
    pub fn uri(&self) -> RemoteFileUri {
        match self.r#type {
            SentryFileType::DebugFile => {
                format!("sentry://project_debug_file/{}", self.file_id).into()
            }
            SentryFileType::ReleaseArtifact => {
                format!("sentry://project_release_artifact/{}", self.file_id).into()
            }
        }
    }

    /// Returns the URL from which to download this object file.
    pub fn url(&self) -> Url {
        match self.r#type {
            SentryFileType::DebugFile => {
                let mut url = self.source.url.clone();
                url.query_pairs_mut().append_pair("id", &self.file_id.0);
                url
            }
            SentryFileType::ReleaseArtifact => {
                let mut url = self.source.url.clone();
                url = url
                    .join(&format!("{}/", &self.file_id.to_string()))
                    .unwrap();
                url.query_pairs_mut().append_pair("download", "1");
                url
            }
        }
    }
}

/// An identifier for a file retrievable from a [`SentrySourceConfig`].
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize)]
pub struct SentryFileId(pub String);

impl fmt::Display for SentryFileId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Available file types stored on Sentry servers.
#[derive(Debug, Clone)]
pub enum SentryFileType {
    /// Native Debug Information File
    DebugFile,
    /// JavaScript Release Artifact (SourceCode/SourceMap)
    ReleaseArtifact,
}
