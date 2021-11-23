use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Error, Result};
use futures::TryStreamExt;
use reqwest::{Body, Client, StatusCode};
use tokio::fs::{self, File};
use tokio::io::{self, AsyncWrite};
use tokio::sync::RwLock;
use tokio_util::io::{ReaderStream, StreamReader};
use url::Url;

use crate::cache::{
    CacheName, FilesystemSharedCacheConfig, GcsSharedCacheConfig, SharedCacheConfig,
};
use crate::logging::LogError;
use crate::services::download::measure_download_time;
use crate::utils::gcs::{request_new_token, GcsError, GcsToken};

use super::cacher::CacheKey;

// TODO: get timeouts from global config?
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct GcsState {
    config: GcsSharedCacheConfig,
    token: RwLock<Option<Arc<GcsToken>>>,
    client: Client,
}

impl GcsState {
    pub fn new(config: GcsSharedCacheConfig) -> Self {
        Self {
            config,
            token: RwLock::new(None),
            client: Client::new(),
        }
    }
    /// Resolves a valid GCS OAuth token.
    ///
    /// If the cache contains a valid token, then this token is returned. Otherwise, a new token is
    /// requested from GCS and stored in the cache.
    async fn get_token(&self) -> Result<Arc<GcsToken>, GcsError> {
        {
            let guard = self.token.read().await;
            if let Some(ref token) = *guard {
                if !token.is_expired() {
                    metric!(counter("shared_cache.gcs.token") += 1, "cache" => "true");
                    return Ok(token.clone());
                }
            }
        }

        // Make sure only 1 token is requested at a time.
        let mut guard = self.token.write().await;
        let token = request_new_token(&self.client, &self.config.source_key).await?;
        metric!(counter("shared_cache.gcs.token") += 1, "cache" => "false");
        let token = Arc::new(token);
        *guard = Some(token.clone());
        Ok(token)
    }

    async fn fetch<W>(&self, key: &SharedCacheKey, writer: &mut W) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let token = self.get_token().await?;

        let mut url = Url::parse("https://www.googleapis.com/download/storage/v1/b?alt=media")
            .map_err(|_| GcsError::InvalidUrl)?;
        // Append path segments manually for proper encoding
        url.path_segments_mut()
            .map_err(|_| GcsError::InvalidUrl)?
            .extend(&[&self.config.bucket, "o", key.gcs_bucket_key().as_ref()]);

        let request = self
            .client
            .get(url)
            .header("authorization", token.bearer_token())
            .send();
        let request = tokio::time::timeout(CONNECT_TIMEOUT, request);
        let request = measure_download_time("shared_cache.gcs.download", request);

        match request.await {
            Ok(Ok(response)) => {
                let status = response.status();
                if status.is_success() {
                    log::trace!("Success hitting shared_cache GCS {}", key.gcs_bucket_key());
                    let stream = response
                        .bytes_stream()
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
                    let mut stream = StreamReader::new(stream);
                    io::copy(&mut stream, writer)
                        .await
                        .map(|_| ())
                        .context("IO Error streaming HTTP bytes to writer")
                } else if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
                    Err(anyhow!(
                        "Insufficient permissions for bucket {}",
                        self.config.bucket
                    ))
                } else {
                    Err(anyhow!(
                        "Error response from GCS for bucket={}, key={}: {} {}",
                        self.config.bucket,
                        key.gcs_bucket_key(),
                        status,
                        status.canonical_reason().unwrap_or("")
                    ))
                }
            }
            Ok(Err(e)) => {
                log::trace!(
                    "Error in shared_cache GCS response for {}",
                    key.gcs_bucket_key()
                );
                // TODO: Would like to use: e.context("Bad GCS response for shared_cache: {}")
                Err(anyhow!(
                    "Bad GCS response for shared_cache: {}",
                    LogError(&e)
                ))
            }
            Err(_) => Err(Error::msg("Timeout from GCS for shared_cache")),
        }
    }

    async fn store(&self, key: SharedCacheKey, src: File) -> Result<()> {
        let token = self.get_token().await?;
        let mut url =
            Url::parse("https://storage.googleapis.com/upload/storage/v1/b?uploadType=media")
                .map_err(|_| GcsError::InvalidUrl)?;
        // Append path segments manually for proper encoding
        url.path_segments_mut()
            .map_err(|_| GcsError::InvalidUrl)?
            .extend(&[&self.config.bucket, "o"]);
        url.query_pairs_mut()
            .append_pair("name", &key.gcs_bucket_key());

        let stream = ReaderStream::new(src);
        let body = Body::wrap_stream(stream);
        let request = self
            .client
            .post(url.clone())
            .header("authorization", token.bearer_token())
            .body(body)
            .send();

        let request = tokio::time::timeout(CONNECT_TIMEOUT, request);

        match request.await {
            Ok(Ok(response)) => {
                let status = response.status();
                if status.is_success() {
                    log::trace!("Success hitting shared_cache GCS {}", key.gcs_bucket_key());
                    Ok(())
                } else if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
                    Err(anyhow!(
                        "Insufficient permissions for bucket {}",
                        self.config.bucket
                    ))
                } else {
                    Err(anyhow!(
                        "Error response from GCS for bucket={}, key={}: {} {}",
                        self.config.bucket,
                        key.gcs_bucket_key(),
                        status,
                        status.canonical_reason().unwrap_or("")
                    ))
                }
            }
            Ok(Err(e)) => {
                log::trace!(
                    "Error in shared_cache GCS response for {}",
                    key.gcs_bucket_key()
                );
                // TODO: Would like to use: e.context("Bad GCS response for shared_cache: {}")
                Err(anyhow!(
                    "Bad GCS response for shared_cache: {}",
                    LogError(&e)
                ))
            }
            Err(_) => Err(Error::msg("Timeout from GCS for shared_cache")),
        }
    }
}

impl FilesystemSharedCacheConfig {
    async fn fetch<W>(&self, key: &SharedCacheKey, writer: &mut W) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let abspath = self.path.join(key.relative_path());
        log::debug!("Fetching debug file from {}", abspath.display());
        let mut file = File::open(abspath).await?;
        io::copy(&mut file, writer).await?;
        Ok(())
    }

    async fn store(&self, key: SharedCacheKey, mut src: File) -> Result<()> {
        let abspath = self.path.join(key.relative_path());
        if let Some(parent_dir) = abspath.parent() {
            fs::create_dir_all(parent_dir).await?;
        }
        let mut dest = File::open(abspath).await?;
        io::copy(&mut src, &mut dest).await?;
        Ok(())
    }
}

/// Key for a shared cache item.
#[derive(Debug, Clone)]
pub struct SharedCacheKey {
    /// The name of the cache.
    pub name: CacheName,
    /// The cache version.
    pub version: u32,
    /// The local cache key.
    pub local_key: CacheKey,
}

impl SharedCacheKey {
    /// The relative path of this cache key within a shared cache.
    fn relative_path(&self) -> PathBuf {
        // Note that this always pushes the version into the path, this is fine since we do
        // not need any backwards compatibility with existing caches for the shared cache.
        let mut path = PathBuf::new();
        path.push(self.name.to_string());
        path.push(self.version.to_string());
        path.push(self.local_key.relative_path());
        path
    }

    /// The [`SharedCacheKey::relative_path`] as a GCS bucket key.
    fn gcs_bucket_key(&self) -> String {
        // All our paths should be UTF-8, we don't construct non-UTF-8 paths.
        match self.relative_path().to_str() {
            Some(s) => s.to_owned(),
            None => {
                log::error!(
                    "Non UTF-8 path in SharedCacheKey: {}",
                    self.relative_path().display()
                );
                self.relative_path().to_string_lossy().into_owned()
            }
        }
    }
}

#[derive(Debug)]
enum SharedCacheBackend {
    Gcs(GcsState),
    Fs(FilesystemSharedCacheConfig),
}

/// A shared cache service.\
///
/// For simplicity in the rest of the application this service always exists, regardless of
/// whether it is configured or not.  If it is not configured this becomes a no-op.
#[derive(Debug)]
pub struct SharedCacheService {
    backend: Option<SharedCacheBackend>,
}

impl SharedCacheService {
    pub fn new(config: Option<SharedCacheConfig>) -> Self {
        // Yes, it is wasteful to create a GcsState when the config does not use GCS.
        // However currently we only have Filesystem as alternative and that is only used
        // for testing purposes.
        let backend = config.map(|cfg| match cfg {
            SharedCacheConfig::Gcs(cfg) => SharedCacheBackend::Gcs(GcsState::new(cfg)),
            SharedCacheConfig::Fs(cfg) => SharedCacheBackend::Fs(cfg),
        });
        Self { backend }
    }

    /// Returns the name of the backend configured.
    fn backend_name(&self) -> &str {
        match self.backend {
            Some(SharedCacheBackend::Fs(_)) => "filesystem",
            Some(SharedCacheBackend::Gcs(_)) => "GCS",
            None => "<not-configured>",
        }
    }

    /// Retrieve a file from the shared cache.
    ///
    /// `cache_path` is the relative path on the shared cache.  `destination` is where to
    /// write the file locally.
    ///
    /// Errors are transparently hidden, either a cache item is available or it is not.
    pub async fn fetch<W>(&self, key: &SharedCacheKey, writer: &mut W) -> bool
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let res = match self.backend {
            Some(SharedCacheBackend::Gcs(ref state)) => state.fetch(key, writer).await,
            Some(SharedCacheBackend::Fs(ref cfg)) => cfg.fetch(key, writer).await,
            None => return false,
        };
        match res {
            Ok(_) => {
                metric!(counter(&format!("services.shared_cache.{}", key.name)) += 1, "hit" => "true");
                true
            }
            Err(err) => {
                metric!(counter(&format!("services.shared_cache.{}", key.name)) += 1, "hit" => "false");
                log::error!(
                    "Error fetching from {} shared cache: {}",
                    self.backend_name(),
                    LogError(&*err)
                );
                false
            }
        }
    }

    /// Place a file on the shared cache, if it does not yet exist there.
    ///
    /// `cache_path` is where the relative path on the shared cache.
    ///
    /// Errors are transparently hidden, this service handles any errors itself.
    pub async fn store(&self, key: SharedCacheKey, src: File) {
        // TODO: concurrency control, handle overload (aka backpressure, but we're not
        // pressing back at all here since we have no return value).

        // TODO: spawn or enqueue here, don't want to be blocking the caller's progress.

        let cache_name = key.name;
        let res = match &self.backend {
            Some(SharedCacheBackend::Gcs(state)) => state.store(key, src).await,
            Some(SharedCacheBackend::Fs(cfg)) => cfg.store(key, src).await,
            None => Ok(()),
        };
        match res {
            Ok(_) => {
                metric!(counter(&format!("shared_caches.{}.file.write", cache_name)) += 1);
            }
            Err(err) => {
                log::error!(
                    "Error storing on {} shared cache: {}",
                    self.backend_name(),
                    LogError(&*err),
                );
            }
        };
    }
}
