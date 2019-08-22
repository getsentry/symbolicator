use std::path::PathBuf;
use std::sync::Arc;

use actix_web::{http::header, web::Bytes, HttpMessage};
use chrono::{DateTime, Duration, Utc};
use failure::ResultExt;
use futures::{future, future::Either, Future, Stream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::percent_encoding::{percent_encode, PATH_SEGMENT_ENCODE_SET};

use crate::service::download::common::{
    prepare_download_paths, DownloadError, DownloadErrorKind, DownloadPath, DownloadedFile,
    ObjectDownloader,
};
use crate::types::{FileType, GcsSourceConfig, GcsSourceKey, ObjectId};
use crate::utils::futures::{RemoteThread, ResultFuture, SendFuture};
use crate::utils::http;

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

fn request_token(auth_jwt: String) -> ResultFuture<Bytes, DownloadError> {
    let future = http::default_client()
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
            DownloadError::io(err)
        })
        .and_then(move |mut response| response.body().map_err(DownloadError::io));

    Box::new(future)
}

fn download(
    source: Arc<GcsSourceConfig>,
    token: Arc<GcsToken>,
    url: String,
    key: String,
    temp_dir: PathBuf,
) -> ResultFuture<Option<DownloadedFile>, DownloadError> {
    let future = http::default_client()
        .get(&url)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", token.access_token),
        )
        .send()
        .map_err(DownloadError::io)
        .then(clone!(temp_dir, |result| match result {
            Ok(mut response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting GCS {} (from {})", &key, source.bucket);
                    let stream = response.take_payload().map_err(DownloadError::io);
                    Either::A(DownloadedFile::streaming(&temp_dir, stream).map(Some))
                } else {
                    log::trace!(
                        "Unexpected status code from GCS {} (from {}): {}",
                        &key,
                        source.bucket,
                        response.status()
                    );
                    Either::B(future::ok(None))
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
                Either::B(future::ok(None))
            }
        }));

    Box::new(future)
}

type TokenCache = lru::LruCache<Arc<GcsSourceKey>, Arc<GcsToken>>;

#[derive(Clone)]
struct GcsDownloaderHandle {
    thread: RemoteThread,
    tokens: Arc<Mutex<TokenCache>>,
}

impl GcsDownloaderHandle {
    fn request_token(&self, source_key: &GcsSourceKey) -> SendFuture<GcsToken, DownloadError> {
        let expires_at = Utc::now() + Duration::minutes(58);
        let auth_jwt = match get_auth_jwt(source_key, expires_at.timestamp() + 30) {
            Ok(auth_jwt) => auth_jwt,
            Err(err) => return Box::new(future::err(err)),
        };

        let response = self
            .thread
            .spawn(move || request_token(auth_jwt))
            .map_err(|e| e.map_canceled(|| DownloadErrorKind::Canceled))
            .and_then(move |data| {
                serde_json::from_slice::<GcsTokenResponse>(&data)
                    .map_err(DownloadError::io)
                    .map(move |token| GcsToken {
                        access_token: token.access_token,
                        expires_at,
                    })
            });

        Box::new(response)
    }

    fn get_token(
        &self,
        source_key: &Arc<GcsSourceKey>,
    ) -> SendFuture<Arc<GcsToken>, DownloadError> {
        if let Some(token) = self.tokens.lock().get(source_key) {
            if token.expires_at < Utc::now() {
                return Box::new(future::ok(token.clone()));
            }
        }

        let tokens = self.tokens.clone();
        let source_key = source_key.clone();
        let future = self.request_token(&source_key).map(move |token| {
            let token = Arc::new(token);
            tokens.lock().put(source_key, token.clone());
            token
        });

        Box::new(future)
    }
}

pub struct GcsDownloader {
    handle: GcsDownloaderHandle,
}

impl GcsDownloader {
    pub fn new(thread: RemoteThread) -> Self {
        Self {
            handle: GcsDownloaderHandle {
                thread,
                tokens: Arc::new(Mutex::new(TokenCache::new(100))),
            },
        }
    }
}

impl ObjectDownloader for GcsDownloader {
    type Config = Arc<GcsSourceConfig>;
    type ListResponse = Result<Vec<DownloadPath>, DownloadError>;
    type DownloadResponse = SendFuture<Option<DownloadedFile>, DownloadError>;

    fn list_files(
        &self,
        source: Self::Config,
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> Self::ListResponse {
        let paths = prepare_download_paths(
            object_id,
            filetypes,
            &source.files.filters,
            source.files.layout,
        );

        Ok(paths.collect())
    }

    fn download(
        &self,
        source: Self::Config,
        download_path: DownloadPath,
        temp_dir: PathBuf,
    ) -> Self::DownloadResponse {
        let key = {
            let prefix = source.prefix.trim_matches(&['/'][..]);
            if prefix.is_empty() {
                download_path.to_string()
            } else {
                format!("{}/{}", prefix, download_path)
            }
        };

        log::debug!("Fetching from GCS: {} (from {})", &key, source.bucket);

        let handle = self.handle.clone();
        let try_response = move || {
            let source = source.clone();
            let key = key.clone();
            let url = format!(
                "https://www.googleapis.com/download/storage/v1/b/{}/o/{}?alt=media",
                percent_encode(source.bucket.as_bytes(), PATH_SEGMENT_ENCODE_SET),
                percent_encode(key.as_bytes(), PATH_SEGMENT_ENCODE_SET),
            );

            handle
                .get_token(&source.source_key)
                .and_then(clone!(handle, temp_dir, |token| {
                    log::debug!("Got valid GCS token: {:?}", &token);

                    handle
                        .thread
                        .spawn(move || download(source, token, url, key, temp_dir))
                        .map_err(|e| e.map_canceled(|| DownloadErrorKind::Canceled))
                }))
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
}
