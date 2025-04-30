//! Access to Google Cloud Storeage

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use gcp_auth::Token;
use jsonwebtoken::errors::Error as JwtError;
use jsonwebtoken::EncodingKey;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use symbolicator_sources::GcsSourceKey;

/// A JWT token usable for GCS.
#[derive(Debug, Clone)]
pub struct GcsToken {
    bearer_token: Arc<str>,
    expires_at: DateTime<Utc>,
}

impl GcsToken {
    /// Whether the token is expired or is still valid.
    pub fn is_expired(&self) -> bool {
        self.expires_at < Utc::now()
    }

    /// The token in the HTTP Bearer-header format, header value only.
    pub fn bearer_token(&self) -> &str {
        &self.bearer_token
    }
}

impl From<&Token> for GcsToken {
    fn from(value: &Token) -> Self {
        GcsToken {
            expires_at: value.expires_at(),
            bearer_token: value.as_str().into(),
        }
    }
}

#[derive(Serialize)]
struct JwtClaims<'s> {
    #[serde(rename = "iss")]
    issuer: &'s str,
    scope: &'s str,
    #[serde(rename = "aud")]
    audience: &'s str,
    #[serde(rename = "exp")]
    expiration: i64,
    #[serde(rename = "iat")]
    issued_at: i64,
}

#[derive(Serialize)]
struct OAuth2Grant {
    grant_type: String,
    assertion: String,
}

#[derive(Deserialize)]
struct GcsTokenResponse {
    access_token: String,
}

#[derive(Debug, Error)]
pub enum GcsError {
    #[error("failed to construct URL")]
    InvalidUrl,
    #[error("failed encoding JWT")]
    Jwt(#[from] JwtError),
    #[error("failed to send authentication request")]
    Auth(#[source] reqwest::Error),
}

/// Returns the URL for an object.
///
/// This can be used to e.g. fetch the objects's metadata.
pub fn object_url(bucket: &str, object: &str) -> Result<Url, GcsError> {
    let mut url = Url::parse("https://storage.googleapis.com/storage/v1")
        .map_err(|_| GcsError::InvalidUrl)?;
    url.path_segments_mut()
        .map_err(|_| GcsError::InvalidUrl)?
        .extend(&["b", bucket, "o", object]);
    Ok(url)
}

/// Returns the download URL for an object.
pub fn download_url(bucket: &str, object: &str) -> Result<Url, GcsError> {
    let mut url = object_url(bucket, object)?;
    url.query_pairs_mut().append_pair("alt", "media");
    Ok(url)
}

/// Returns the JWT key parsed from a string.
///
/// Because Google provides this key in JSON format a lot of users just copy-paste this key
/// directly, leaving the escaped newlines from the JSON-encoding in place.  In normal
/// base64 this should not occur so we pre-process the key to convert these back to real
/// newlines, ensuring they are in the correct PEM format.
fn key_from_string(key: &str) -> Result<EncodingKey, JwtError> {
    let buffer = key.replace("\\n", "\n");
    EncodingKey::from_rsa_pem(buffer.as_bytes())
}

/// Computes a JWT authentication assertion for the given GCS bucket.
fn get_auth_jwt(source_key: &GcsSourceKey, expiration: i64) -> Result<String, JwtError> {
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);

    let jwt_claims = JwtClaims {
        issuer: &source_key.client_email,
        scope: "https://www.googleapis.com/auth/devstorage.read_only",
        audience: "https://www.googleapis.com/oauth2/v4/token",
        expiration,
        issued_at: Utc::now().timestamp(),
    };

    let key = key_from_string(&source_key.private_key)?;

    jsonwebtoken::encode(&header, &jwt_claims, &key)
}

/// Requests a new GCS OAuth token.
pub async fn request_new_token(
    client: &Client,
    source_key: &GcsSourceKey,
) -> Result<GcsToken, GcsError> {
    let expires_at = Utc::now() + Duration::minutes(58);
    let auth_jwt = get_auth_jwt(source_key, expires_at.timestamp() + 30)?;

    let request = client
        .post("https://www.googleapis.com/oauth2/v4/token")
        .form(&OAuth2Grant {
            grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
            assertion: auth_jwt,
        });

    let response = request.send().await.map_err(|err| {
        tracing::debug!("Failed to authenticate against gcs: {}", err);
        GcsError::Auth(err)
    })?;

    let token = response
        .json::<GcsTokenResponse>()
        .await
        .map_err(GcsError::Auth)?;
    let bearer_token = format!("Bearer {}", token.access_token).into();

    Ok(GcsToken {
        bearer_token,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_from_string() {
        let creds = symbolicator_test::gcs_source_key!();

        let key = key_from_string(&creds.private_key);
        assert!(key.is_ok());

        let json_key = serde_json::to_string(&creds.private_key).unwrap();
        let json_like_key = json_key.trim_matches('"');

        let key = key_from_string(json_like_key);
        assert!(key.is_ok());
    }

    #[test]
    fn test_object_url() {
        let url = object_url("bucket", "object/with/a/path").unwrap();
        assert_eq!(
            url.as_str(),
            "https://storage.googleapis.com/storage/v1/b/bucket/o/object%2Fwith%2Fa%2Fpath"
        );
    }

    #[test]
    fn test_download_url() {
        let url = download_url("bucket", "object/with/a/path").unwrap();
        assert_eq!(
            url.as_str(),
            "https://storage.googleapis.com/storage/v1/b/bucket/o/object%2Fwith%2Fa%2Fpath?alt=media"
        );
    }
}
