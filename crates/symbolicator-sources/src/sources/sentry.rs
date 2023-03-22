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
#[derive(Clone)]
pub struct SentryRemoteFile {
    /// The underlying [`SentrySourceConfig`].
    pub source: Arc<SentrySourceConfig>,
    pub(crate) file_id: SentryFileId,
    url: Url,
}

impl fmt::Debug for SentryRemoteFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SentryRemoteFile")
            .field("id", &self.file_id.0)
            .field("url", &self.url.as_str())
            .finish()
    }
}

impl From<SentryRemoteFile> for RemoteFile {
    fn from(source: SentryRemoteFile) -> Self {
        Self::Sentry(source)
    }
}

impl SentryRemoteFile {
    /// Creates a new [`SentryRemoteFile`].
    pub fn new(source: Arc<SentrySourceConfig>, file_id: SentryFileId, url: Option<Url>) -> Self {
        let url = url.unwrap_or_else(|| {
            let mut url = source.url.clone();
            url.query_pairs_mut().append_pair("id", &file_id.0);
            url
        });

        Self {
            source,
            file_id,
            url,
        }
    }

    /// Gives a synthetic [`RemoteFileUri`] for this file.
    pub fn uri(&self) -> RemoteFileUri {
        format!("sentry://project_debug_file/{}", self.file_id).into()
    }

    /// Returns the URL from which to download this object file.
    pub fn url(&self) -> Url {
        self.url.clone()
    }

    pub(crate) fn host(&self) -> String {
        self.url.to_string()
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
