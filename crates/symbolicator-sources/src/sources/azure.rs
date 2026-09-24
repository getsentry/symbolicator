use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{CommonSourceConfig, RemoteFile, RemoteFileUri, SourceId, SourceLocation};

/// Configuration for an Azure Blob Storage container.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AzureSourceConfig {
    /// Unique source identifier.
    pub id: SourceId,

    /// Name of the Azure storage account.
    pub account: String,

    /// Name of the blob container.
    pub container: String,

    /// A path from the root of the container where files are located.
    #[serde(default)]
    pub prefix: String,

    /// Authorization information for this container. Needs read access.
    #[serde(flatten)]
    pub source_key: Arc<AzureSourceKey>,

    /// Configuration common to all sources.
    #[serde(flatten)]
    pub files: CommonSourceConfig,
}

/// The Azure-specific [`RemoteFile`].
#[derive(Debug, Clone)]
pub struct AzureRemoteFile {
    /// The underlying [`AzureSourceConfig`].
    pub source: Arc<AzureSourceConfig>,
    pub(crate) location: SourceLocation,
}

impl From<AzureRemoteFile> for RemoteFile {
    fn from(source: AzureRemoteFile) -> Self {
        Self::Azure(source)
    }
}

impl AzureRemoteFile {
    /// Creates a new [`AzureRemoteFile`].
    pub fn new(source: Arc<AzureSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the blob name within the container.
    pub fn key(&self) -> String {
        self.location.prefix(&self.source.prefix)
    }

    pub(crate) fn host(&self) -> String {
        format!("{}.blob.core.windows.net", self.source.account)
    }

    /// Returns the `https://` URI from which to download this object file.
    pub fn uri(&self) -> RemoteFileUri {
        RemoteFileUri::from_parts(
            "https",
            &self.host(),
            &format!("{}/{}", self.source.container, self.key()),
        )
    }
}

/// Azure service principal credentials.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct AzureSourceKey {
    /// The Microsoft Entra tenant ID.
    pub tenant_id: String,

    /// The application (client) ID.
    pub client_id: String,

    /// The client secret.
    pub client_secret: AzureClientSecret,
}

/// An Azure client secret.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct AzureClientSecret(pub Arc<str>);

impl fmt::Debug for AzureClientSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<azure client secret>")
    }
}
