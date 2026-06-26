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

    /// Credentials to use.
    #[serde(flatten)]
    pub credentials: SentryCredentials,
}

/// The Sentry-specific [`RemoteFile`].
#[derive(Clone)]
pub struct SentryRemoteFile {
    /// The underlying [`SentrySourceConfig`].
    pub source: Arc<SentrySourceConfig>,
    pub(crate) file_id: SentryFileId,
    use_credentials: bool,
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
    pub fn new(
        source: Arc<SentrySourceConfig>,
        use_credentials: bool,
        file_id: SentryFileId,
        url: Option<Url>,
    ) -> Self {
        let url = url.unwrap_or_else(|| {
            let mut url = source.url.clone();
            url.query_pairs_mut().append_pair("id", &file_id.0);
            url
        });

        Self {
            source,
            file_id,
            use_credentials,
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

    /// Indicates that credentials should be provided for this request.
    pub fn use_credentials(&self) -> bool {
        self.use_credentials
    }
}

/// An identifier for a file retrievable from a [`SentrySourceConfig`].
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SentryFileId(pub Arc<str>);

impl fmt::Display for SentryFileId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Credentials for a Sentry source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SentryCredentials {
    /// A bearer authorization token.
    Token(SentryToken),

    /// A raw Cookie header value for an existing Sentry session.
    Cookies(SentryCookies),
}

impl From<SentryToken> for SentryCredentials {
    fn from(value: SentryToken) -> Self {
        Self::Token(value)
    }
}

impl From<SentryCookies> for SentryCredentials {
    fn from(value: SentryCookies) -> Self {
        Self::Cookies(value)
    }
}

/// A Sentry authentication token.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SentryToken(pub String);

impl fmt::Debug for SentryToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<sentry token>")
    }
}

/// A raw Cookie header value for an existing Sentry session.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SentryCookies(pub String);

impl fmt::Debug for SentryCookies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<sentry cookies>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sentry_source_credentials_token() {
        let c: SentrySourceConfig = serde_json::from_str(
            r#"{
            "id": "foobar",
            "url": "http://foo.bar",
            "token": "123"
}"#,
        )
        .unwrap();

        insta::assert_json_snapshot!(c, @r#"
        {
          "id": "foobar",
          "url": "http://foo.bar/",
          "token": "123"
        }
        "#);
    }

    #[test]
    fn test_sentry_source_credentialsoken_cookies() {
        let c: SentrySourceConfig = serde_json::from_str(
            r#"{
            "id": "foobar",
            "url": "http://foo.bar",
            "cookies": "foo=bar; asd=123"
}"#,
        )
        .unwrap();

        insta::assert_json_snapshot!(c, @r#"
        {
          "id": "foobar",
          "url": "http://foo.bar/",
          "cookies": "foo=bar; asd=123"
        }
        "#);
    }
}
