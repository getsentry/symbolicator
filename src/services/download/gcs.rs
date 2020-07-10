//! Support to download from Google Cloud Storage buckets.
//!
//! Specifically this supports the [`GcsSourceConfig`] source.
//!
//! [`GcsSourceConfig`]: ../../../sources/struct.GcsSourceConfig.html

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use actix_web::{client, HttpMessage};
use chrono::{DateTime, Duration, Utc};
use failure::{Fail, ResultExt};
use futures::compat::Future01CompatExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use url::percent_encoding::{percent_encode, PATH_SEGMENT_ENCODE_SET};
use url::Url;

use super::{DownloadError, DownloadErrorKind, DownloadStatus};
use crate::sources::{FileType, GcsSourceConfig, GcsSourceKey, SourceFileId, SourceLocation};
use crate::types::ObjectId;

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

fn key_from_string(mut s: &str) -> Result<Vec<u8>, DownloadError> {
    if s.starts_with("-----BEGIN PRIVATE KEY-----") {
        s = s.splitn(5, "-----").nth(2).unwrap();
    }

    let bytes = &s
        .as_bytes()
        .iter()
        .cloned()
        .filter(|b| !b.is_ascii_whitespace())
        .collect::<Vec<u8>>();

    Ok(base64::decode(bytes).context(DownloadErrorKind::Io)?)
}

fn get_auth_jwt(source_key: &GcsSourceKey, expiration: i64) -> Result<String, DownloadError> {
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

    Ok(jsonwebtoken::encode(&header, &jwt_claims, pkcs8).context(DownloadErrorKind::Io)?)
}

async fn request_new_token(source_key: &GcsSourceKey) -> Result<GcsToken, DownloadError> {
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
        DownloadError::from(DownloadErrorKind::Io)
    })?;
    let token = response
        .json::<GcsTokenResponse>()
        .compat()
        .await
        .map_err(|e| e.context(DownloadErrorKind::Io))?;
    Ok(GcsToken {
        access_token: token.access_token,
        expires_at,
    })
}

async fn get_token(source_key: &Arc<GcsSourceKey>) -> Result<Arc<GcsToken>, DownloadError> {
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

pub async fn download_source(
    source: Arc<GcsSourceConfig>,
    download_path: SourceLocation,
    destination: PathBuf,
) -> Result<DownloadStatus, DownloadError> {
    let key = source.get_key(&download_path);
    log::debug!("Fetching from GCS: {} (from {})", &key, source.bucket);
    let token = get_token(&source.source_key).await?;
    log::debug!("Got valid GCS token: {:?}", &token);

    let url = Url::parse(&format!(
        "https://www.googleapis.com/download/storage/v1/b/{}/o/{}?alt=media",
        percent_encode(source.bucket.as_bytes(), PATH_SEGMENT_ENCODE_SET),
        percent_encode(key.as_bytes(), PATH_SEGMENT_ENCODE_SET),
    ))
    .unwrap();

    let mut headers = BTreeMap::new();
    headers.insert(
        "authorization".to_owned(),
        format!("Bearer {}", token.access_token),
    );

    super::http::download_url(url, &headers, destination).await
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
