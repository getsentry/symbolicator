//! Support to download from Google Cloud Storage buckets.
//!
//! Specifically this supports the [`GcsSourceConfig`] source.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use futures::prelude::*;
use jsonwebtoken::EncodingKey;
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use super::locations::SourceLocation;
use super::{DownloadError, DownloadStatus, ObjectFileSource, ObjectFileSourceUri};
use crate::sources::{FileType, GcsSourceConfig, GcsSourceKey};
use crate::types::ObjectId;
use crate::utils::futures as future_utils;

/// An LRU cache for GCS OAuth tokens.
type GcsTokenCache = lru::LruCache<Arc<GcsSourceKey>, Arc<GcsToken>>;

/// Maximum number of cached GCS OAuth tokens.
///
/// This number defines the size of the internal cache for GCS authentication and should be higher
/// than expected concurrency across GCS buckets. If this number is too low, the downloader will
/// re-authenticate between every request.
///
/// This can be monitored with the `source.gcs.token.requests` and `source.gcs.token.cached` counter
/// metrics.
const GCS_TOKEN_CACHE_SIZE: usize = 100;

/// The GCS-specific [`ObjectFileSource`].
#[derive(Debug, Clone)]
pub struct GcsObjectFileSource {
    pub source: Arc<GcsSourceConfig>,
    pub location: SourceLocation,
}

impl From<GcsObjectFileSource> for ObjectFileSource {
    fn from(source: GcsObjectFileSource) -> Self {
        Self::Gcs(source)
    }
}

impl GcsObjectFileSource {
    pub fn new(source: Arc<GcsSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the S3 key.
    ///
    /// This is equivalent to the pathname within the bucket.
    pub fn key(&self) -> String {
        self.location.prefix(&self.source.prefix)
    }

    /// Returns the `gs://` URI from which to download this object file.
    pub fn uri(&self) -> ObjectFileSourceUri {
        format!("gs://{}/{}", self.source.bucket, self.key()).into()
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

#[derive(Debug)]
struct GcsToken {
    access_token: String,
    expires_at: DateTime<Utc>,
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

/// Downloader implementation that supports the [`GcsSourceConfig`] source.
#[derive(Debug)]
pub struct GcsDownloader {
    token_cache: Mutex<GcsTokenCache>,
    client: reqwest::Client,
}

impl GcsDownloader {
    pub fn new(client: Client) -> Self {
        Self {
            token_cache: Mutex::new(GcsTokenCache::new(GCS_TOKEN_CACHE_SIZE)),
            client,
        }
    }

    /// Requests a new GCS OAuth token.
    async fn request_new_token(&self, source_key: &GcsSourceKey) -> Result<GcsToken, GcsError> {
        let expires_at = Utc::now() + Duration::minutes(58);
        let auth_jwt = get_auth_jwt(source_key, expires_at.timestamp() + 30)?;

        let request = self
            .client
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

    /// Resolves a valid GCS OAuth token.
    ///
    /// If the cache contains a valid token, then this token is returned. Otherwise, a new token is
    /// requested from GCS and stored in the cache.
    async fn get_token(&self, source_key: &Arc<GcsSourceKey>) -> Result<Arc<GcsToken>, GcsError> {
        if let Some(token) = self.token_cache.lock().get(source_key) {
            if token.expires_at >= Utc::now() {
                metric!(counter("source.gcs.token.cached") += 1);
                return Ok(token.clone());
            }
        }

        let source_key = source_key.clone();
        let token = self.request_new_token(&source_key).await?;
        metric!(counter("source.gcs.token.requests") += 1);
        let token = Arc::new(token);
        self.token_cache.lock().put(source_key, token.clone());
        Ok(token)
    }

    pub async fn download_source(
        &self,
        file_source: GcsObjectFileSource,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let key = file_source.key();
        log::debug!(
            "Fetching from GCS: {} (from {})",
            &key,
            file_source.source.bucket
        );
        let token = self.get_token(&file_source.source.source_key).await?;
        log::debug!("Got valid GCS token");

        let mut url = Url::parse("https://www.googleapis.com/download/storage/v1/b?alt=media")
            .map_err(|_| GcsError::InvalidUrl)?;
        // Append path segements manually for proper encoding
        url.path_segments_mut()
            .map_err(|_| GcsError::InvalidUrl)?
            .extend(&[&file_source.source.bucket, "o", &key]);

        let response = future_utils::retry(|| {
            self.client
                .get(url.clone())
                .header("authorization", format!("Bearer {}", token.access_token))
                .send()
        });

        match response.await {
            Ok(response) => {
                if response.status().is_success() {
                    log::trace!(
                        "Success hitting GCS {} (from {})",
                        &key,
                        file_source.source.bucket
                    );
                    let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

                    super::download_stream(file_source, stream, destination).await
                } else {
                    log::trace!(
                        "Unexpected status code from GCS {} (from {}): {}",
                        &key,
                        &file_source.source.bucket,
                        response.status()
                    );
                    Ok(DownloadStatus::NotFound)
                }
            }
            Err(e) => {
                log::trace!(
                    "Skipping response from GCS {} (from {}): {} ({:?})",
                    &key,
                    &file_source.source.bucket,
                    &e,
                    &e
                );
                Ok(DownloadStatus::NotFound)
            }
        }
    }

    pub fn list_files(
        &self,
        source: Arc<GcsSourceConfig>,
        filetypes: &'static [FileType],
        object_id: ObjectId,
    ) -> Vec<ObjectFileSource> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id: &object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| GcsObjectFileSource::new(source.clone(), loc).into())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::locations::SourceLocation;
    use super::*;

    use crate::sources::{CommonSourceConfig, DirectoryLayoutType, SourceId};
    use crate::test;
    use crate::types::ObjectType;

    use sha1::{Digest as _, Sha1};

    fn gcs_source_key() -> Option<GcsSourceKey> {
        let private_key = std::env::var("SENTRY_SYMBOLICATOR_GCS_PRIVATE_KEY").ok()?;
        let client_email = std::env::var("SENTRY_SYMBOLICATOR_GCS_CLIENT_EMAIL").ok()?;

        if private_key.is_empty() || client_email.is_empty() {
            None
        } else {
            Some(GcsSourceKey {
                private_key,
                client_email,
            })
        }
    }

    macro_rules! gcs_source_key {
        () => {
            match gcs_source_key() {
                Some(key) => key,
                None => {
                    println!("Skipping due to missing SENTRY_SYMBOLICATOR_GCS_PRIVATE_KEY or SENTRY_SYMBOLICATOR_GCS_CLIENT_EMAIL");
                    return;
                }
            }
        }
    }

    fn gcs_source(source_key: GcsSourceKey) -> Arc<GcsSourceConfig> {
        Arc::new(GcsSourceConfig {
            id: SourceId::new("gcs-test"),
            bucket: "sentryio-system-symbols-0".to_owned(),
            prefix: "/ios".to_owned(),
            source_key: Arc::new(source_key),
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        })
    }

    #[test]
    fn test_list_files() {
        test::setup();

        let source = gcs_source(gcs_source_key!());
        let downloader = GcsDownloader::new(Client::new());

        let object_id = ObjectId {
            code_id: Some("e514c9464eed3be5943a2c61d9241fad".parse().unwrap()),
            code_file: Some("/usr/lib/system/libdyld.dylib".to_owned()),
            debug_id: Some("e514c946-4eed-3be5-943a-2c61d9241fad".parse().unwrap()),
            debug_file: Some("libdyld.dylib".to_owned()),
            object_type: ObjectType::Macho,
        };

        let list = downloader.list_files(source, &[FileType::MachCode], object_id);
        assert_eq!(list.len(), 1);

        assert!(list[0]
            .uri()
            .to_string()
            .ends_with("e5/14c9464eed3be5943a2c61d9241fad/executable"));
    }

    #[tokio::test]
    async fn test_download_complete() {
        test::setup();

        let source = gcs_source(gcs_source_key!());
        let downloader = GcsDownloader::new(Client::new());

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        // Location of /usr/lib/system/libdyld.dylib
        let source_location = SourceLocation::new("e5/14c9464eed3be5943a2c61d9241fad/executable");
        let file_source = GcsObjectFileSource::new(source, source_location);

        let download_status = downloader
            .download_source(file_source, target_path.clone())
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::Completed);
        assert!(target_path.exists());

        let hash = Sha1::digest(&std::fs::read(target_path).unwrap());
        let hash = format!("{:x}", hash);
        assert_eq!(hash, "206e63c06da135be1858dde03778caf25f8465b8");
    }

    #[tokio::test]
    async fn test_download_missing() {
        test::setup();

        let source = gcs_source(gcs_source_key!());
        let downloader = GcsDownloader::new(Client::new());

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = GcsObjectFileSource::new(source, source_location);

        let download_status = downloader
            .download_source(file_source, target_path.clone())
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::NotFound);
        assert!(!target_path.exists());
    }

    #[tokio::test]
    async fn test_download_invalid_credentials() {
        test::setup();

        let broken_credentials = GcsSourceKey {
            private_key: "".to_owned(),
            client_email: "".to_owned(),
        };

        let source = gcs_source(broken_credentials);
        let downloader = GcsDownloader::new(Client::new());

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = GcsObjectFileSource::new(source, source_location);

        downloader
            .download_source(file_source, target_path.clone())
            .await
            .expect_err("authentication should fail");

        assert!(!target_path.exists());
    }

    #[test]
    fn test_key_from_string() {
        let creds = gcs_source_key!();

        let key = key_from_string(&creds.private_key);
        assert!(key.is_ok());

        let json_key = serde_json::to_string(&creds.private_key).unwrap();
        let json_like_key = json_key.trim_matches('"');

        let key = key_from_string(json_like_key);
        assert!(key.is_ok());
    }

    // TODO: Test credential caching.
}
