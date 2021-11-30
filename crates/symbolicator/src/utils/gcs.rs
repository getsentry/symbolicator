//! Access to Google Cloud Storeage

use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::EncodingKey;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sources::GcsSourceKey;

/// A JWT token usable for GCS.
#[derive(Debug)]
pub struct GcsToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

impl GcsToken {
    /// Whether the token is expired or is still valid.
    pub fn is_expired(&self) -> bool {
        self.expires_at < Utc::now()
    }

    /// The token in the HTTP Bearer-header format, header value only.
    pub fn bearer_token(&self) -> String {
        format!("Bearer {}", self.access_token)
    }
}

#[derive(Serialize)]
struct JwtClaims {
    #[serde(rename = "iss")]
    issuer: String,
    scope: String,
    #[serde(rename = "aud")]
    audience: String,
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
    #[error("failed decoding key")]
    Base64(#[from] base64::DecodeError),
    #[error("failed to construct URL")]
    InvalidUrl,
    #[error("failed encoding JWT")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("failed to send authentication request")]
    Auth(#[source] reqwest::Error),
}

/// Returns the JWT key parsed from a string.
///
/// Because Google provides this key in JSON format a lot of users just copy-paste this key
/// directly, leaving the escaped newlines from the JSON-encoding in place.  In normal
/// base64 this should not occur so we pre-process the key to convert these back to real
/// newlines, ensuring they are in the correct PEM format.
fn key_from_string(key: &str) -> Result<EncodingKey, jsonwebtoken::errors::Error> {
    let buffer = key.replace("\\n", "\n");
    EncodingKey::from_rsa_pem(buffer.as_bytes())
}

/// Computes a JWT authentication assertion for the given GCS bucket.
fn get_auth_jwt(source_key: &GcsSourceKey, expiration: i64) -> Result<String, GcsError> {
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);

    let jwt_claims = JwtClaims {
        issuer: source_key.client_email.clone(),
        scope: "https://www.googleapis.com/auth/devstorage.read_only".into(),
        audience: "https://www.googleapis.com/oauth2/v4/token".into(),
        expiration,
        issued_at: Utc::now().timestamp(),
    };

    let key = key_from_string(&source_key.private_key)?;

    Ok(jsonwebtoken::encode(&header, &jwt_claims, &key)?)
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
        log::debug!("Failed to authenticate against gcs: {}", err);
        GcsError::Auth(err)
    })?;

    let token = response
        .json::<GcsTokenResponse>()
        .await
        .map_err(GcsError::Auth)?;

    Ok(GcsToken {
        access_token: token.access_token,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test;

    #[test]
    fn test_key_from_string() {
        let creds = test::gcs_source_key!();

        let key = key_from_string(&creds.private_key);
        assert!(key.is_ok());

        let json_key = serde_json::to_string(&creds.private_key).unwrap();
        let json_like_key = json_key.trim_matches('"');

        let key = key_from_string(json_like_key);
        assert!(key.is_ok());
    }
}
