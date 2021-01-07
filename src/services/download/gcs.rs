//! Support to download from Google Cloud Storage buckets.
//!
//! Specifically this supports the [`GcsSourceConfig`] source.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use futures::prelude::*;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use url::percent_encoding::{percent_encode, PATH_SEGMENT_ENCODE_SET};

use super::{DownloadError, DownloadStatus};
use crate::sources::{FileType, GcsSourceConfig, GcsSourceKey, SourceFileId, SourceLocation};
use crate::types::ObjectId;
use crate::utils::futures::delay;

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
    #[error("failed encoding JWT")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("failed to send authentication request")]
    Auth(#[source] reqwest::Error),
}

/// Parses the given private key string into its binary representation.
///
/// Returns `Ok` on success. Returns `GcsError::Base64`, if the key cannot be parsed.
fn key_from_string(mut s: &str) -> Result<Vec<u8>, GcsError> {
    if s.starts_with("-----BEGIN PRIVATE KEY-----") {
        s = s.splitn(5, "-----").nth(2).unwrap();
    }

    let bytes = &s
        .as_bytes()
        .iter()
        .cloned()
        .filter(|b| !b.is_ascii_whitespace())
        .collect::<Vec<u8>>();

    Ok(base64::decode(bytes)?)
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
    let pkcs8 = jsonwebtoken::Key::Pkcs8(&key);

    Ok(jsonwebtoken::encode(&header, &jwt_claims, pkcs8)?)
}

/// Downloader implementation that supports the [`GcsSourceConfig`] source.
#[derive(Debug)]
pub struct GcsDownloader {
    token_cache: Mutex<GcsTokenCache>,
    client: reqwest::Client,
}

impl GcsDownloader {
    pub fn new() -> Self {
        let token_cache = Mutex::new(GcsTokenCache::new(GCS_TOKEN_CACHE_SIZE));
        let client = reqwest::Client::new();
        Self {
            token_cache,
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
            // TODO(ja): check if this still holds
            // for some inexplicable reason we otherwise get gzipped data back that actix-web
            // client has no idea what to do with.
            .header("accept-encoding", "identity")
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
        source: Arc<GcsSourceConfig>,
        download_path: SourceLocation,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let key = source.get_key(&download_path);
        log::debug!("Fetching from GCS: {} (from {})", &key, source.bucket);
        let token = self.get_token(&source.source_key).await?;
        log::debug!("Got valid GCS token: {:?}", &token);

        let url = format!(
            "https://www.googleapis.com/download/storage/v1/b/{}/o/{}?alt=media",
            percent_encode(source.bucket.as_bytes(), PATH_SEGMENT_ENCODE_SET),
            percent_encode(key.as_bytes(), PATH_SEGMENT_ENCODE_SET),
        );

        let mut backoff = ExponentialBackoff::from_millis(10).map(jitter).take(3);
        let response = loop {
            let result = self
                .client
                .get(&url)
                .header("authorization", format!("Bearer {}", token.access_token))
                .send()
                .await;

            match backoff.next() {
                Some(duration) if result.is_err() => delay(duration).await,
                _ => break result,
            }
        };

        match response {
            Ok(response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting GCS {} (from {})", &key, source.bucket);
                    let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

                    super::download_stream(
                        SourceFileId::Gcs(source, download_path),
                        stream,
                        destination,
                    )
                    .await
                } else {
                    log::trace!(
                        "Unexpected status code from GCS {} (from {}): {}",
                        &key,
                        source.bucket,
                        response.status()
                    );
                    Ok(DownloadStatus::NotFound)
                }
            }
            Err(e) => {
                log::trace!(
                    "Skipping response from GCS {} (from {}): {} ({:?})",
                    &key,
                    source.bucket,
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
    ) -> Vec<SourceFileId> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id: &object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| SourceFileId::Gcs(source.clone(), loc))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::sources::{CommonSourceConfig, DirectoryLayoutType, SourceId};
    use crate::test;
    use crate::types::ObjectType;

    use super::*;
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
        let downloader = GcsDownloader::new();

        let object_id = ObjectId {
            code_id: Some("e514c9464eed3be5943a2c61d9241fad".parse().unwrap()),
            code_file: Some("/usr/lib/system/libdyld.dylib".to_owned()),
            debug_id: Some("e514c946-4eed-3be5-943a-2c61d9241fad".parse().unwrap()),
            debug_file: Some("libdyld.dylib".to_owned()),
            object_type: ObjectType::Macho,
        };

        let list = downloader.list_files(source, &[FileType::MachCode], object_id);
        assert_eq!(list.len(), 1);

        assert_eq!(
            list[0].location(),
            SourceLocation::new("e5/14c9464eed3be5943a2c61d9241fad/executable")
        );
    }

    #[test]
    fn test_download_complete() {
        test::setup();

        let source = gcs_source(gcs_source_key!());
        let downloader = GcsDownloader::new();

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        // Location of /usr/lib/system/libdyld.dylib
        let source_location = SourceLocation::new("e5/14c9464eed3be5943a2c61d9241fad/executable");

        let download_status = test::block_fn(|| {
            downloader.download_source(source, source_location, target_path.clone())
        })
        .unwrap();

        assert_eq!(download_status, DownloadStatus::Completed);
        assert!(target_path.exists());

        let hash = Sha1::digest(&std::fs::read(target_path).unwrap());
        let hash = format!("{:x}", hash);
        assert_eq!(hash, "206e63c06da135be1858dde03778caf25f8465b8");
    }

    #[test]
    fn test_download_missing() {
        test::setup();

        let source = gcs_source(gcs_source_key!());
        let downloader = GcsDownloader::new();

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");

        let download_status = test::block_fn(|| {
            downloader.download_source(source, source_location, target_path.clone())
        })
        .unwrap();

        assert_eq!(download_status, DownloadStatus::NotFound);
        assert!(!target_path.exists());
    }

    #[test]
    fn test_download_invalid_credentials() {
        test::setup();

        let broken_credentials = GcsSourceKey {
            private_key: "".to_owned(),
            client_email: "".to_owned(),
        };

        let source = gcs_source(broken_credentials);
        let downloader = GcsDownloader::new();

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");

        test::block_fn(|| downloader.download_source(source, source_location, target_path.clone()))
            .expect_err("authentication should fail");

        assert!(!target_path.exists());
    }

    // TODO: Test credential caching.
}
