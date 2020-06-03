//! Support to download from Google Cloud Storage buckets.
//!
//! Specifically this supports the [`GcsSourceConfig`] source.
//!
//! [`GcsSourceConfig`]: ../../../sources/struct.GcsSourceConfig.html

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use actix_web::{client, HttpMessage};
use chrono::{DateTime, Duration, Utc};
use failure::{Fail, ResultExt};
use futures01::future;
use futures01::prelude::*;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::percent_encoding::{percent_encode, PATH_SEGMENT_ENCODE_SET};

use super::types::{DownloadError, DownloadErrorKind, DownloadStatus, DownloadStream};
use crate::sources::{GcsSourceConfig, GcsSourceKey, SourceLocation};

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

/// Download from a Google Cloud Storage source.
///
/// See [`Downloader::download`] for the semantics of the file being written at `dest`.
///
/// ['Downloader::download`]: ../func.download.html
pub fn download_source(
    source: Arc<GcsSourceConfig>,
    location: SourceLocation,
    dest: PathBuf,
) -> Box<dyn Future<Item = DownloadStatus, Error = DownloadError>> {
    // All file I/O in this function is blocking!
    let source2 = source.clone();
    let ret =
        download_from_source(source, &location).and_then(move |maybe_stream| match maybe_stream {
            Some(stream) => {
                log::trace!(
                    "Downloading file for HTTP source {id} location {loc}",
                    id = source2.id,
                    loc = location
                );
                let file =
                    tryf!(std::fs::File::create(&dest).context(DownloadErrorKind::BadDestination));
                let fut = stream
                    .fold(file, |mut file, chunk| {
                        file.write_all(chunk.as_ref())
                            .context(DownloadErrorKind::Write)
                            .map_err(DownloadError::from)
                            .map(|_| file)
                    })
                    .and_then(|_| Ok(DownloadStatus::Completed));
                Box::new(fut) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
            }
            None => {
                let fut = future::ok(DownloadStatus::NotFound);
                Box::new(fut) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
            }
        });
    Box::new(ret) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
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

fn request_new_token(
    source_key: &GcsSourceKey,
) -> Box<dyn Future<Item = GcsToken, Error = DownloadError>> {
    let expires_at = Utc::now() + Duration::minutes(58);
    let auth_jwt = match get_auth_jwt(source_key, expires_at.timestamp() + 30) {
        Ok(auth_jwt) => auth_jwt,
        Err(err) => return Box::new(Err(err).into_future()),
    };

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

    let response = response
        .map_err(|err| {
            log::debug!("Failed to authenticate against gcs: {}", err);
            DownloadError::from(DownloadErrorKind::Io)
        })
        .and_then(move |resp| {
            resp.json::<GcsTokenResponse>()
                .map_err(|e| e.context(DownloadErrorKind::Io).into())
                .map(move |token| GcsToken {
                    access_token: token.access_token,
                    expires_at,
                })
        });

    Box::new(response)
}

fn get_token(
    source_key: &Arc<GcsSourceKey>,
) -> Box<dyn Future<Item = Arc<GcsToken>, Error = DownloadError>> {
    if let Some(token) = GCS_TOKENS.lock().get(source_key) {
        if token.expires_at >= Utc::now() {
            metric!(counter("source.gcs.token.cached") += 1);
            return Box::new(Ok(token.clone()).into_future());
        }
    }

    let source_key = source_key.clone();
    Box::new(request_new_token(&source_key).map(move |token| {
        metric!(counter("source.gcs.token.requests") += 1);
        let token = Arc::new(token);
        GCS_TOKENS.lock().put(source_key, token.clone());
        token
    }))
}

fn download_from_source(
    source: Arc<GcsSourceConfig>,
    download_path: &SourceLocation,
) -> Box<dyn Future<Item = Option<DownloadStream>, Error = DownloadError>> {
    let key = {
        let prefix = source.prefix.trim_matches(&['/'][..]);
        if prefix.is_empty() {
            download_path.0.clone()
        } else {
            format!("{}/{}", prefix, download_path.0)
        }
    };
    log::debug!("Fetching from GCS: {} (from {})", &key, source.bucket);

    let try_response = move || {
        let source = source.clone();
        let key = key.clone();
        let url = format!(
            "https://www.googleapis.com/download/storage/v1/b/{}/o/{}?alt=media",
            percent_encode(source.bucket.as_bytes(), PATH_SEGMENT_ENCODE_SET),
            percent_encode(key.as_bytes(), PATH_SEGMENT_ENCODE_SET),
        );
        get_token(&source.source_key)
            .and_then(move |token| {
                log::debug!("Got valid GCS token: {:?}", &token);
                let mut builder = client::get(&url);
                builder.header("authorization", format!("Bearer {}", token.access_token));
                builder
                    .finish()
                    .unwrap()
                    .send()
                    .map_err(|err| err.context(DownloadErrorKind::Io).into())
            })
            .then(move |result| match result {
                Ok(response) => {
                    if response.status().is_success() {
                        log::trace!("Success hitting GCS {} (from {})", &key, source.bucket);
                        Ok(Some(Box::new(
                            response
                                .payload()
                                .map_err(|e| e.context(DownloadErrorKind::Io).into()),
                        )
                            as Box<dyn Stream<Item = _, Error = _>>))
                    } else {
                        log::trace!(
                            "Unexpected status code from GCS {} (from {}): {}",
                            &key,
                            source.bucket,
                            response.status()
                        );
                        Ok(None)
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
                    Ok(None)
                }
            })
    };

    let response = Retry::spawn(
        ExponentialBackoff::from_millis(10).map(jitter).take(3),
        try_response,
    );

    Box::new(response.map_err(|e| match e {
        tokio_retry::Error::OperationError(e) => e,
        e => panic!("{}", e),
    }))
}
