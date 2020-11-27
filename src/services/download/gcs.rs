//! Support to download from Google Cloud Storage buckets.
//!
//! Specifically this supports the [`GcsSourceConfig`] source.

use std::path::PathBuf;
use std::sync::Arc;

use actix_web::error::JsonPayloadError;
use actix_web::{client, HttpMessage};
use chrono::{DateTime, Duration, Utc};
use client::SendRequestError;
use failure::Fail;
use futures::compat::{Future01CompatExt, Stream01CompatExt};
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

lazy_static::lazy_static! {
    static ref GCS_TOKENS: Mutex<lru::LruCache<Arc<GcsSourceKey>, Arc<GcsToken>>> =
        Mutex::new(lru::LruCache::new(100));
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
    #[error("failed encoding JWT")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("failed to parse JSON response")]
    Json(#[from] failure::Compat<JsonPayloadError>),
    #[error("failed to send authentication request")]
    Auth(#[from] failure::Compat<SendRequestError>),
}

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

async fn request_new_token(source_key: &GcsSourceKey) -> Result<GcsToken, GcsError> {
    let expires_at = Utc::now() + Duration::minutes(58);
    let auth_jwt = get_auth_jwt(source_key, expires_at.timestamp() + 30)?;

    let mut builder = client::post("https://www.googleapis.com/oauth2/v4/token");
    // for some inexplicable reason we otherwise get gzipped data back that actix-web
    // client has no idea what to do with.
    builder.header("accept-encoding", "identity");
    let response = builder
        .form(&OAuth2Grant {
            grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
            assertion: auth_jwt,
        })
        .unwrap()
        .send();

    let response = response.compat().await.map_err(|err| {
        log::debug!("Failed to authenticate against gcs: {}", err);
        err.compat()
    })?;
    let token = response
        .json::<GcsTokenResponse>()
        .compat()
        .await
        .map_err(Fail::compat)?;
    Ok(GcsToken {
        access_token: token.access_token,
        expires_at,
    })
}

async fn get_token(source_key: &Arc<GcsSourceKey>) -> Result<Arc<GcsToken>, GcsError> {
    if let Some(token) = GCS_TOKENS.lock().get(source_key) {
        if token.expires_at >= Utc::now() {
            metric!(counter("source.gcs.token.cached") += 1);
            return Ok(token.clone());
        }
    }

    let source_key = source_key.clone();
    let token = request_new_token(&source_key).await?;
    metric!(counter("source.gcs.token.requests") += 1);
    let token = Arc::new(token);
    GCS_TOKENS.lock().put(source_key, token.clone());
    Ok(token)
}

async fn start_request(
    url: &str,
    token: &GcsToken,
) -> Result<client::ClientResponse, client::SendRequestError> {
    let mut builder = client::get(&url);
    builder.header("authorization", format!("Bearer {}", token.access_token));
    builder.finish().unwrap().send().compat().await
}

pub async fn download_source(
    source: Arc<GcsSourceConfig>,
    download_path: SourceLocation,
    destination: PathBuf,
) -> Result<DownloadStatus, DownloadError> {
    let key = source.get_key(&download_path);
    log::debug!("Fetching from GCS: {} (from {})", &key, source.bucket);
    let token = get_token(&source.source_key).await?;
    log::debug!("Got valid GCS token: {:?}", &token);

    let url = format!(
        "https://www.googleapis.com/download/storage/v1/b/{}/o/{}?alt=media",
        percent_encode(source.bucket.as_bytes(), PATH_SEGMENT_ENCODE_SET),
        percent_encode(key.as_bytes(), PATH_SEGMENT_ENCODE_SET),
    );

    let mut backoff = ExponentialBackoff::from_millis(10).map(jitter).take(3);
    let response = loop {
        let result = start_request(&url, &token).await;
        match backoff.next() {
            Some(duration) if result.is_err() => delay(duration).await,
            _ => break result,
        }
    };

    match response {
        Ok(response) => {
            if response.status().is_success() {
                log::trace!("Success hitting GCS {} (from {})", &key, source.bucket);
                let stream = response
                    .payload()
                    .compat()
                    .map(|i| i.map_err(DownloadError::stream));
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
