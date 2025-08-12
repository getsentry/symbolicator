use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{CommonSourceConfig, RemoteFile, RemoteFileUri, SourceId, SourceLocation};

/// Configuration for a GCS symbol buckets.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GcsSourceConfig {
    /// Unique source identifier.
    pub id: SourceId,

    /// Name of the GCS bucket.
    pub bucket: String,

    /// A path from the root of the bucket where files are located.
    #[serde(default)]
    pub prefix: String,

    /// Authorization information for this bucket. Needs read access.
    #[serde(flatten)]
    pub source_authorization: GcsSourceAuthorization,

    /// Configuration common to all sources.
    #[serde(flatten)]
    pub files: CommonSourceConfig,
}

/// The GCS-specific [`RemoteFile`].
#[derive(Debug, Clone)]
pub struct GcsRemoteFile {
    /// The underlying [`GcsSourceConfig`].
    pub source: Arc<GcsSourceConfig>,
    pub(crate) location: SourceLocation,
}

impl From<GcsRemoteFile> for RemoteFile {
    fn from(source: GcsRemoteFile) -> Self {
        Self::Gcs(source)
    }
}

impl GcsRemoteFile {
    /// Creates a new [`GcsRemoteFile`].
    pub fn new(source: Arc<GcsSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the GCS key.
    ///
    /// This is equivalent to the pathname within the bucket.
    pub fn key(&self) -> String {
        self.location.prefix(&self.source.prefix)
    }

    /// Returns the `gs://` URI from which to download this object file.
    pub fn uri(&self) -> RemoteFileUri {
        RemoteFileUri::from_parts("gs", &self.source.bucket, &self.key())
    }

    pub(crate) fn host(&self) -> String {
        self.source.bucket.clone()
    }
}

/// GCS authorization information.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum GcsSourceAuthorization {
    /// (email, private_key) pair used for authorization.
    SourceKey(Arc<GcsSourceKey>),
    /// Authorization token used directly.
    SourceToken(GcsSourceToken),
}

/// GCS authorization credentials.
///
/// These are used to obtain a token which is then used for GCS communication.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct GcsSourceKey {
    /// Gcs authorization key.
    pub private_key: GcsPrivateKey,

    /// The client email.
    pub client_email: String,
}

/// A GCS private key.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct GcsPrivateKey(pub Arc<str>);

impl fmt::Debug for GcsPrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<gcs private key>")
    }
}

/// GCS authorization token.
///
/// This token will be used directly to authorize against GCS.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct GcsSourceToken {
    /// GCS bearer token.
    pub bearer_token: GcsBearerToken,
}

/// A GCS bearer token.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct GcsBearerToken(pub Arc<str>);

impl fmt::Debug for GcsBearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<gcs bearer token>")
    }
}

#[cfg(test)]
mod test {
    use crate::GcsSourceConfig;

    #[test]
    fn test_parse_request_source_key() {
        let json = r#"
        {
            "id": "some-source-id",
            "bucket": "some-bucket",
            "prefix": "some-prefix",
            "private_key": "some-private-key",
            "client_email": "some-client@email"
        }"#;
        let config: GcsSourceConfig = serde_json::from_str(json).unwrap();
        insta::assert_debug_snapshot!(config, @r###"
        GcsSourceConfig {
            id: SourceId(
                "some-source-id",
            ),
            bucket: "some-bucket",
            prefix: "some-prefix",
            source_authorization: SourceKey(
                GcsSourceKey {
                    private_key: <gcs private key>,
                    client_email: "some-client@email",
                },
            ),
            files: CommonSourceConfig {
                filters: SourceFilters {
                    filetypes: [],
                    path_patterns: [],
                    requires_checksum: false,
                },
                layout: DirectoryLayout {
                    ty: Native,
                    casing: Default,
                },
                is_public: false,
                has_index: false,
            },
        }
        "###);
    }

    #[test]
    fn test_parse_request_token() {
        let json = r#"
        {
            "id": "some-source-id",
            "bucket": "some-bucket",
            "prefix": "some-prefix",
            "bearer_token": "some-token"
        }"#;
        let config: GcsSourceConfig = serde_json::from_str(json).unwrap();
        insta::assert_debug_snapshot!(config, @r###"
        GcsSourceConfig {
            id: SourceId(
                "some-source-id",
            ),
            bucket: "some-bucket",
            prefix: "some-prefix",
            source_authorization: SourceToken(
                GcsSourceToken {
                    bearer_token: <gcs bearer token>,
                },
            ),
            files: CommonSourceConfig {
                filters: SourceFilters {
                    filetypes: [],
                    path_patterns: [],
                    requires_checksum: false,
                },
                layout: DirectoryLayout {
                    ty: Native,
                    casing: Default,
                },
                is_public: false,
                has_index: false,
            },
        }
        "###);
    }
}
