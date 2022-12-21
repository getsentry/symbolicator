use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{CommonSourceConfig, RemoteFile, RemoteFileUri, SourceId, SourceLocation};

/// Configuration for symbol server HTTP endpoints.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HttpSourceConfig {
    /// Unique source identifier.
    pub id: SourceId,

    /// Absolute URL of the symbol server.
    pub url: Url,

    /// Additional headers to be sent to the symbol server with every request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    /// Configuration common to all sources.
    #[serde(flatten)]
    pub files: CommonSourceConfig,
}

/// The HTTP-specific [`RemoteDif`].
#[derive(Debug, Clone)]
pub struct HttpRemoteFile {
    pub source: Arc<HttpSourceConfig>,
    pub location: SourceLocation,
}

impl From<HttpRemoteFile> for RemoteFile {
    fn from(source: HttpRemoteFile) -> Self {
        Self::Http(source)
    }
}

impl HttpRemoteFile {
    pub fn new(source: Arc<HttpSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    pub fn uri(&self) -> RemoteFileUri {
        match self.url() {
            Ok(url) => url.as_ref().into(),
            Err(_) => "".into(),
        }
    }

    /// Returns the URL from which to download this object file.
    pub fn url(&self) -> anyhow::Result<Url> {
        self.location.to_url(&self.source.url)
    }
}
