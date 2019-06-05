use std::sync::Arc;

use actix_web::{client, HttpMessage};
use chrono::{DateTime, Duration, Utc};
use failure::{Fail, ResultExt};
use futures::{Future, IntoFuture, Stream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::percent_encoding::{percent_encode, PATH_SEGMENT_ENCODE_SET};

use crate::actors::objects::common::prepare_download_paths;
use crate::actors::objects::{DownloadPath, DownloadStream, FileId, ObjectError, ObjectErrorKind};
use crate::types::{FileType, GcsSourceConfig, GcsSourceKey, ObjectId};

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
    Ok(base64::decode(
        &s.as_bytes()
            .iter()
            .cloned()
            .filter(|b| !b.is_ascii_whitespace())
            .collect::<Vec<u8>>(),
    )
    .context(ObjectErrorKind::Io)?)
}

fn get_auth_jwt(source_key: &GcsSourceKey, expiration: i64) -> Result<String, ObjectError> {
    let jwt_claims = JwtClaims {
        issuer: source_key.client_email.clone(),
        scope: "https://www.googleapis.com/auth/devstorage.read_only".into(),
        audience: "https://www.googleapis.com/oauth2/v4/token".into(),
        expiration,
        issued_at: Utc::now().timestamp(),
    };

    Ok(jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &jwt_claims,
        jsonwebtoken::Pkcs8::from(&key_from_string(&source_key.private_key)?),
    )
    .context(ObjectErrorKind::Io)?)
}

fn request_new_token(
    source_key: &GcsSourceKey,
) -> Box<Future<Item = GcsToken, Error = ObjectError>> {
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
            ObjectError::from(ObjectErrorKind::Io)
        })
        .and_then(move |resp| {
            resp.json::<GcsTokenResponse>()
                .map_err(|e| e.context(ObjectErrorKind::Io).into())
                .map(move |token| GcsToken {
                    access_token: token.access_token,
                    expires_at,
                })
        });

    Box::new(response)
}

fn get_token(
    source_key: &Arc<GcsSourceKey>,
) -> Box<Future<Item = Arc<GcsToken>, Error = ObjectError>> {
    if let Some(token) = GCS_TOKENS.lock().get(source_key) {
        if token.expires_at < Utc::now() {
            return Box::new(Ok(token.clone()).into_future());
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
) -> Box<Future<Item = Vec<FileId>, Error = ObjectError>> {
    let ids = prepare_download_paths(
        object_id,
        filetypes,
        &source.files.filters,
        source.files.layout,
    )
    .map(|download_path| FileId::Gcs(source.clone(), download_path))
    .collect();

    Box::new(Ok(ids).into_future())
}

pub(super) fn download_from_source(
    source: Arc<GcsSourceConfig>,
    download_path: &DownloadPath,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
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
                    .map_err(|err| err.context(ObjectErrorKind::Io).into())
            })
            .then(move |result| match result {
                Ok(response) => {
                    if response.status().is_success() {
                        log::trace!("Success hitting GCS {} (from {})", &key, source.bucket);
                        Ok(Some(DownloadStream::FutureStream(Box::new(
                            response
                                .payload()
                                .map_err(|e| e.context(ObjectErrorKind::Io).into()),
                        )
                            as Box<dyn Stream<Item = _, Error = _>>)))
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
        ExponentialBackoff::from_millis(100).map(jitter).take(3),
        try_response,
    );

    Box::new(response.map_err(|e| match e {
        tokio_retry::Error::OperationError(e) => e,
        e => panic!("{}", e),
    }))
}
