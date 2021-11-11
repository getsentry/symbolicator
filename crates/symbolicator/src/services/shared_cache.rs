use std::io;
use std::path::Path;
use std::sync::Arc;

use chrono::{Duration, Utc};
use parking_lot::Mutex;
use reqwest::{Client, StatusCode};
use tokio::fs;
use url::Url;

use crate::cache::{FilesystemSharedCacheConfig, GcsSharedCacheConfig, SharedCacheConfig};
use crate::services::download::gcs::{
    get_auth_jwt, GcsError, GcsToken, GcsTokenCache, GcsTokenResponse, OAuth2Grant,
    GCS_TOKEN_CACHE_SIZE,
};
use crate::services::download::{DownloadError, DownloadStatus, SourceLocation};
use crate::sources::GcsSourceKey;

#[derive(Debug)]
struct GcsSharedCacheService {
    token_cache: Mutex<GcsTokenCache>,
    client: reqwest::Client,
}

impl GcsSharedCacheService {
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

    /// Downloads a file hosted on GCS.
    ///
    /// # Directly thrown errors
    /// - [`GcsError::InvalidUrl`]
    /// - [`DownloadError::Reqwest`]
    /// - [`DownloadError::Rejected`]
    /// - [`DownloadError::Canceled`]
    pub async fn fetch(
        &self,
        shared_cache: &GcsSharedCacheConfig,
        file_source: SourceLocation,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        let key = file_source.prefix(&shared_cache.prefix);
        let bucket = shared_cache.bucket.clone();
        log::debug!("Fetching from GCS: {} (from {})", &key, bucket);
        let token = self.get_token(&shared_cache.source_key).await?;
        log::debug!("Got valid GCS token");

        let mut url = Url::parse("https://www.googleapis.com/download/storage/v1/b?alt=media")
            .map_err(|_| GcsError::InvalidUrl)?;
        // Append path segments manually for proper encoding
        url.path_segments_mut()
            .map_err(|_| GcsError::InvalidUrl)?
            .extend(&[&bucket, "o", &key]);

        let source = RemoteDif::from(file_source);
        let request = self
            .client
            .get(url.clone())
            .header("authorization", format!("Bearer {}", token.access_token))
            .send();
        let request = tokio::time::timeout(self.connect_timeout, request);
        let request = super::measure_download_time(source.source_metric_key(), request);

        match request.await {
            Ok(Ok(response)) => {
                if response.status().is_success() {
                    log::trace!("Success hitting GCS {} (from {})", &key, bucket);

                    let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

                    super::download_stream(&source, stream, destination, timeout).await
                } else if matches!(
                    response.status(),
                    StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
                ) {
                    log::debug!(
                        "Insufficient permissions to download from GCS {} (from {})",
                        &key,
                        &bucket,
                    );
                    Err(DownloadError::Permissions)
                // If it's a client error, chances are either it's a 404 or it's permission-related.
                } else if response.status().is_client_error() {
                    log::debug!(
                        "Unexpected client error status code from GCS {} (from {}): {}",
                        &key,
                        &bucket,
                        response.status()
                    );
                    Ok(DownloadStatus::NotFound)
                } else {
                    log::debug!(
                        "Unexpected status code from GCS {} (from {}): {}",
                        &key,
                        &bucket,
                        response.status()
                    );
                    Err(DownloadError::Rejected(response.status()))
                }
            }
            Ok(Err(e)) => {
                log::debug!(
                    "Skipping response from GCS {} (from {}): {}",
                    &key,
                    &bucket,
                    &e
                );
                Err(DownloadError::Reqwest(e))
            }
            Err(_) => {
                // Timeout
                Err(DownloadError::Canceled)
            }
        }
    }

    /// Uploads a file to GCS.
    pub async fn store(
        &self,
        shared_cache: &GcsSharedCacheConfig,
        destination: SourceLocation,
        contents: &[u8],
    ) -> Result<DownloadStatus, DownloadError> {
        let key = destination.prefix(&shared_cache.prefix);
        let bucket = shared_cache.bucket.clone();
        log::debug!("Fetching from GCS: {} (from {})", &key, bucket);
        let token = self.get_token(&shared_cache.source_key).await?;
        log::debug!("Got valid GCS token");

        let mut url =
            Url::parse("https://storage.googleapis.com/upload/storage/v1/b?uploadType=media")
                .map_err(|_| GcsError::InvalidUrl)?;
        // Append path segments manually for proper encoding
        url.path_segments_mut()
            .map_err(|_| GcsError::InvalidUrl)?
            .extend(&[&bucket, "o"]);
        url.query_pairs_mut().append_pair("name", &key);

        let request = self
            .client
            .post(url.clone())
            .header("authorization", format!("Bearer {}", token.access_token))
            .body(contents.to_owned()) // TODO: ideally don't allocate here, but lifetime needs to be 'static
            .send();

        match request.await {
            Ok(response) => {
                if response.status().is_success() {
                    Ok(DownloadStatus::Completed)
                } else if matches!(
                    response.status(),
                    StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
                ) {
                    log::debug!(
                        "Insufficient permissions to download from GCS {} (from {})",
                        &key,
                        &bucket,
                    );
                    Err(DownloadError::Permissions)
                // If it's a client error, chances are either it's a 404 or it's permission-related.
                } else if response.status().is_client_error() {
                    log::debug!(
                        "Unexpected client error status code from GCS {} (from {}): {}",
                        &key,
                        &bucket,
                        response.status()
                    );
                    Ok(DownloadStatus::NotFound)
                } else {
                    log::debug!(
                        "Unexpected status code from GCS {} (from {}): {}",
                        &key,
                        &bucket,
                        response.status()
                    );
                    Err(DownloadError::Rejected(response.status()))
                }
            }
            Err(e) => {
                log::debug!(
                    "Skipping response from GCS {} (from {}): {}",
                    &key,
                    &bucket,
                    &e
                );
                Err(DownloadError::Reqwest(e))
            }
        }
    }
}

#[derive(Debug, Default)]
struct FilesystemSharedCacheService {}

impl FilesystemSharedCacheService {
    pub async fn fetch(
        &self,
        shared_cache: &FilesystemSharedCacheConfig,
        file_source: SourceLocation,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        // All file I/O in this function is blocking!
        let abspath = shared_cache.path.join(file_source.path());
        log::debug!("Fetching debug file from {:?}", abspath);
        match fs::copy(abspath, destination).await {
            Ok(_) => Ok(DownloadStatus::Completed),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => Ok(DownloadStatus::NotFound),
                _ => Err(DownloadError::Io(e)),
            },
        }
    }

    pub async fn store(
        &self,
        shared_cache: &FilesystemSharedCacheConfig,
        destination: SourceLocation,
        contents: &[u8],
    ) -> Result<DownloadStatus, DownloadError> {
        let abspath = shared_cache.path.join(destination.path());
        if let Some(parent_dir) = abspath.parent() {
            std::fs::create_dir_all(parent_dir).map_err(DownloadError::Io)?;
        }
        match std::fs::write(abspath, contents) {
            Ok(()) => Ok(DownloadStatus::Completed),
            Err(e) => Err(DownloadError::Io(e)),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SharedCacheService {
    gcs: GcsSharedCacheService,
    filesystem: FilesystemSharedCacheService,
}

impl SharedCacheService {
    pub async fn fetch(
        &self,
        shared_cache: &SharedCacheConfig,
        file_source: SourceLocation,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        match shared_cache {
            SharedCacheConfig::Gcs(gcs) => self.gcs.fetch(gcs, file_source, destination).await,
            SharedCacheConfig::Fs(fs) => self.filesystem.fetch(fs, file_source, destination).await,
        }
    }

    pub async fn store(
        &self,
        shared_cache: &SharedCacheConfig,
        destination: SourceLocation,
        contents: &[u8],
    ) -> Result<DownloadStatus, DownloadError> {
        match shared_cache {
            SharedCacheConfig::Gcs(gcs) => self.gcs.store(gcs, destination, contents).await,
            SharedCacheConfig::Fs(fs) => self.filesystem.store(fs, destination, contents).await,
        }
    }
}
