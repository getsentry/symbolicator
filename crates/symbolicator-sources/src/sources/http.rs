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

    /// If true, it should be possible to download from this source
    /// even if SSL certificates can't be verified.
    ///
    /// Don't use this lightly!
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

/// The HTTP-specific [`RemoteFile`].
#[derive(Debug, Clone)]
pub struct HttpRemoteFile {
    /// The underlying [`HttpSourceConfig`].
    pub source: Arc<HttpSourceConfig>,
    pub(crate) location: SourceLocation,
    /// Additional HTTP headers to send with a symbol request.
    pub headers: BTreeMap<String, String>,
}

impl From<HttpRemoteFile> for RemoteFile {
    fn from(source: HttpRemoteFile) -> Self {
        Self::Http(source)
    }
}

impl HttpRemoteFile {
    /// Creates a new [`HttpRemoteFile`].
    pub fn new(source: Arc<HttpSourceConfig>, location: SourceLocation) -> Self {
        Self {
            source,
            location,
            headers: Default::default(),
        }
    }

    /// Creates a new [`HttpRemoteFile`] from the given [`Url`].
    /// This internally creates a bogus [`HttpSourceConfig`].
    pub fn from_url(url: Url, verify_ssl: bool) -> Self {
        let source = Arc::new(HttpSourceConfig {
            id: SourceId::new("web-scraping"),
            url,
            headers: Default::default(),
            files: Default::default(),
            accept_invalid_certs: !verify_ssl,
        });
        let location = SourceLocation::new("");

        HttpRemoteFile::new(source, location)
    }

    pub(crate) fn uri(&self) -> RemoteFileUri {
        match self.url() {
            Ok(url) => url.as_ref().into(),
            Err(_) => "".into(),
        }
    }

    /// Returns the URL from which to download this object file.
    pub fn url(&self) -> anyhow::Result<Url> {
        self.location.to_url(&self.source.url)
    }

    pub(crate) fn host(&self) -> String {
        self.source.url.host_str().unwrap_or_default().to_string()
    }
}
