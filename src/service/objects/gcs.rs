use std::sync::Arc;

use actix_web::{http::header, HttpMessage};
use chrono::{DateTime, Duration, Utc};
use failure::ResultExt;
use futures::{future, Future, Stream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::percent_encoding::{percent_encode, PATH_SEGMENT_ENCODE_SET};

use crate::service::objects::common::{prepare_download_paths, DownloadPath};
use crate::service::objects::{DownloadStream, FileId, ObjectError, ObjectErrorKind};
use crate::types::{FileType, GcsSourceConfig, GcsSourceKey, ObjectId};
use crate::utils::http;

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

fn key_from_string(mut s: &str) -> Result<Vec<u8>, ObjectError> {
    if s.starts_with("-----BEGIN PRIVATE KEY-----") {
        s = s.splitn(5, "-----").nth(2).unwrap();
    }

    let bytes = &s
        .as_bytes()
        .iter()
        .cloned()
        .filter(|b| !b.is_ascii_whitespace())
        .collect::<Vec<u8>>();

    Ok(base64::decode(bytes).context(ObjectErrorKind::Io)?)
}

fn get_auth_jwt(source_key: &GcsSourceKey, expiration: i64) -> Result<String, ObjectError> {
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

    Ok(jsonwebtoken::encode(&header, &jwt_claims, pkcs8).context(ObjectErrorKind::Io)?)
}

fn request_new_token(
    source_key: &GcsSourceKey,
) -> Box<dyn Future<Item = GcsToken, Error = ObjectError>> {
    let expires_at = Utc::now() + Duration::minutes(58);
    let auth_jwt = match get_auth_jwt(source_key, expires_at.timestamp() + 30) {
        Ok(auth_jwt) => auth_jwt,
        Err(err) => return Box::new(future::err(err)),
    };

    let response = http::default_client()
        .post("https://www.googleapis.com/oauth2/v4/token")
        // for some inexplicable reason we otherwise get gzipped data back that actix-web client has
        // no idea what to do with.
        .header(header::ACCEPT_ENCODING, "identity")
        .send_form(&OAuth2Grant {
            grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
            assertion: auth_jwt,
        })
        .map_err(|err| {
            log::debug!("Failed to authenticate against GCS: {}", err);
            ObjectError::io(err)
        })
        .and_then(move |mut resp| {
            resp.json::<GcsTokenResponse>()
                .map_err(ObjectError::io)
                .map(move |token| GcsToken {
                    access_token: token.access_token,
                    expires_at,
                })
        });

    Box::new(response)
}

fn get_token(
    source_key: &Arc<GcsSourceKey>,
) -> Box<dyn Future<Item = Arc<GcsToken>, Error = ObjectError>> {
    if let Some(token) = GCS_TOKENS.lock().get(source_key) {
        if token.expires_at < Utc::now() {
            return Box::new(future::ok(token.clone()));
        }
    }

    let source_key = source_key.clone();
    Box::new(request_new_token(&source_key).map(move |token| {
        let token = Arc::new(token);
        GCS_TOKENS.lock().put(source_key, token.clone());
        token
    }))
}

pub(super) fn prepare_downloads(
    source: &Arc<GcsSourceConfig>,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> Box<dyn Future<Item = Vec<FileId>, Error = ObjectError>> {
    let ids = prepare_download_paths(
        object_id,
        filetypes,
        &source.files.filters,
        source.files.layout,
    )
    .map(|download_path| FileId::Gcs(source.clone(), download_path))
    .collect();

    Box::new(future::ok(ids))
}

pub(super) fn download_from_source(
    source: Arc<GcsSourceConfig>,
    download_path: &DownloadPath,
) -> Box<dyn Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    let key = {
        let prefix = source.prefix.trim_matches(&['/'][..]);
        if prefix.is_empty() {
            download_path.to_string()
        } else {
            format!("{}/{}", prefix, download_path)
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
                http::default_client()
                    .get(&url)
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", token.access_token),
                    )
                    .send()
                    .map_err(ObjectError::io)
            })
            .then(move |result| match result {
                Ok(mut response) => {
                    if response.status().is_success() {
                        log::trace!("Success hitting GCS {} (from {})", &key, source.bucket);
                        let stream = Box::new(response.take_payload().map_err(ObjectError::io));
                        Ok(Some(DownloadStream::FutureStream(stream)))
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
        tokio_retry::Error::TimerError(_) => unreachable!(),
    }))
}
