use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Error, Result};
use futures::TryStreamExt;
use reqwest::{Body, Client, StatusCode};
use tokio::fs::{self, File};
use tokio::io::{self, AsyncSeekExt, AsyncWrite};
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

    async fn fetch<W>(&self, key: &SharedCacheKey, writer: &mut W) -> Result<Option<()>>
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
                match status {
                    _ if status.is_success() => {
                        log::trace!("Success hitting shared_cache GCS {}", key.gcs_bucket_key());
                        let stream = response
                            .bytes_stream()
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
                        let mut stream = StreamReader::new(stream);
                        let res = io::copy(&mut stream, writer)
                            .await
                            .map(|_| ())
                            .context("IO Error streaming HTTP bytes to writer");
                        Some(res).transpose()
                    }
                    StatusCode::NOT_FOUND => Ok(None),
                    StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => Err(anyhow!(
                        "Insufficient permissions for bucket {}",
                        self.config.bucket
                    )),
                    _ => Err(anyhow!(
                        "Error response from GCS for bucket={}, key={}: {} {}",
                        self.config.bucket,
                        key.gcs_bucket_key(),
                        status,
                        status.canonical_reason().unwrap_or("")
                    )),
                }
            }
            Ok(Err(e)) => {
                log::trace!(
                    "Error in shared_cache GCS response for {}",
                    key.gcs_bucket_key()
                );
                Err(e).context("Bad GCS response for shared_cache")
            }
            Err(_) => Err(Error::msg("Timeout from GCS for shared_cache")),
        }
    }

    async fn store(&self, key: SharedCacheKey, mut src: File) -> Result<SharedCacheStoreResult> {
        src.rewind().await?;
        let token = self.get_token().await?;
        let mut url =
            Url::parse("https://storage.googleapis.com/upload/storage/v1/b?uploadType=media")
                .map_err(|_| GcsError::InvalidUrl)?;
        // Append path segments manually for proper encoding
        url.path_segments_mut()
            .map_err(|_| GcsError::InvalidUrl)?
            .extend(&[&self.config.bucket, "o"]);
        url.query_pairs_mut()
            .append_pair("name", &key.gcs_bucket_key())
            // Upload only if it's not already there
            .append_pair("ifGenerationMatch", "0");

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
                match status {
                    successful if successful.is_success() => {
                        log::trace!("Success hitting shared_cache GCS {}", key.gcs_bucket_key());
                        Ok(SharedCacheStoreResult::Written)
                    }
                    StatusCode::PRECONDITION_FAILED => Ok(SharedCacheStoreResult::Skipped),
                    StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => Err(anyhow!(
                        "Insufficient permissions for bucket {}",
                        self.config.bucket
                    )),
                    _ => Err(anyhow!(
                        "Error response from GCS for bucket={}, key={}: {} {}",
                        self.config.bucket,
                        key.gcs_bucket_key(),
                        status,
                        status.canonical_reason().unwrap_or("")
                    )),
                }
            }
            Ok(Err(err)) => {
                log::trace!(
                    "Error in shared_cache GCS response for {}",
                    key.gcs_bucket_key()
                );
                Err(err).context("Bad GCS response for shared_cache")
            }
            Err(_) => Err(Error::msg("Timeout from GCS for shared_cache")),
        }
    }
}

impl FilesystemSharedCacheConfig {
    async fn fetch<W>(&self, key: &SharedCacheKey, writer: &mut W) -> Result<Option<()>>
    where
        W: AsyncWrite + Unpin,
    {
        let abspath = self.path.join(key.relative_path());
        log::debug!("Fetching debug file from {}", abspath.display());
        let mut file = match File::open(abspath).await {
            Ok(file) => file,
            Err(err) => match err.kind() {
                io::ErrorKind::NotFound => return Ok(None),
                _ => return Err(err).context("Failed to open file in shared cache"),
            },
        };
        match io::copy(&mut file, writer).await {
            Ok(_) => Ok(Some(())),
            Err(err) => Err(err).context("Failed to copy file from shared cache"),
        }
    }

    async fn store(&self, key: SharedCacheKey, mut src: File) -> Result<SharedCacheStoreResult> {
        let abspath = self.path.join(key.relative_path());
        if let Some(parent_dir) = abspath.parent() {
            fs::create_dir_all(parent_dir)
                .await
                .context("Failed to create parent directories")?;
        }
        if abspath.as_path().exists() {
            return Ok(SharedCacheStoreResult::Skipped);
        }

        let mut dest = File::create(abspath)
            .await
            .context("Failed to create cache file")?;
        src.rewind().await.context("Failed to seek to src start")?;
        io::copy(&mut src, &mut dest)
            .await
            .context("Failed to copy data into file")?;
        Ok(SharedCacheStoreResult::Written)
    }
}

/// The result of an attempt to write an entry to the shared cache.
#[derive(Debug, Clone, Copy)]
enum SharedCacheStoreResult {
    /// Successfully written to the cache as a new entry.
    Written,
    /// Skipped writing the item as it was already on the cache.
    Skipped,
}

impl AsRef<str> for SharedCacheStoreResult {
    fn as_ref(&self) -> &str {
        match self {
            SharedCacheStoreResult::Written => "written",
            SharedCacheStoreResult::Skipped => "skipped",
        }
    }
}

impl fmt::Display for SharedCacheStoreResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.as_ref())
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

/// A shared cache service.
///
/// For simplicity in the rest of the application this service always exists, regardless of
/// whether it is configured or not.  If it is not configured calls to it's methods become a
/// no-op.
#[derive(Debug)]
pub struct SharedCacheService {
    backend: Option<SharedCacheBackend>,
}

impl SharedCacheService {
    pub fn new(config: Option<SharedCacheConfig>) -> Self {
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
    /// Looks up the `key` in the shared cache, if found the cache contents will be written
    /// to `writer`.
    ///
    /// Returns `true` if the shared cache was found and written to the `writer`.  If the
    /// shared cache was not found nothing will have been written to `writer`.
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
            Ok(Some(_)) => {
                metric!(
                    counter("services.shared_cache.store") += 1,
                    "cache" => key.name.as_ref(),
                    "hit" => "true",
                );
                true
            }
            Ok(None) => {
                metric!(
                    counter("services.shared_cache.store") += 1,
                    "cache" => key.name.as_ref(),
                    "hit" => "false",
                );
                false
            }
            Err(err) => {
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
    /// Errors are transparently hidden, this service handles any errors itself.
    pub async fn store(&self, key: SharedCacheKey, src: File) {
        // TODO: concurrency control, handle overload (aka backpressure, but we're not
        // pressing back at all here since we have no return value).

        // TODO: spawn or enqueue here, don't want to be blocking the caller's progress.

        let cache_name = key.name;
        let res = match &self.backend {
            Some(SharedCacheBackend::Gcs(state)) => state.store(key, src).await,
            Some(SharedCacheBackend::Fs(cfg)) => cfg.store(key, src).await,
            None => Ok(SharedCacheStoreResult::Skipped),
        };
        match res {
            Ok(op) => {
                metric!(
                    counter("services.shared_caches.store") += 1,
                    "cache" => cache_name.as_ref(),
                    "write" => op.as_ref(),
                );
            }
            Err(err) => {
                log::error!(
                    "Error storing file on {} shared cache: {}",
                    self.backend_name(),
                    LogError(&*err),
                );
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    use crate::test;
    use crate::types::Scope;

    use super::*;

    #[tokio::test]
    async fn test_noop_fetch() {
        test::setup();
        let svc = SharedCacheService::new(None);
        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };
        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;
        assert!(!ret);
    }

    #[tokio::test]
    async fn test_noop_store() {
        test::setup();
        let svc = SharedCacheService::new(None);
        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };
        let stdfile = tempfile::tempfile().unwrap();
        let file = File::from_std(stdfile);

        svc.store(key, file).await;
    }

    #[tokio::test]
    async fn test_filesystem_fetch_found() -> Result<()> {
        test::setup();
        let dir = test::tempdir();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };
        let cache_path = dir.path().join(key.relative_path());
        fs::create_dir_all(cache_path.parent().unwrap()).await?;
        fs::write(&cache_path, b"cache data").await?;

        let cfg = SharedCacheConfig::Fs(FilesystemSharedCacheConfig {
            path: dir.path().to_path_buf(),
        });
        let svc = SharedCacheService::new(Some(cfg));

        // This mimics how Cacher::compute creates this file.
        let temp_file = NamedTempFile::new_in(&dir)?;
        let stdfile = temp_file.reopen()?;
        let mut file = File::from_std(stdfile);

        let ret = svc.fetch(&key, &mut file).await;

        assert!(ret);
        file.rewind().await?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await?;
        assert_eq!(buf, b"cache data");
        Ok(())
    }

    #[tokio::test]
    async fn test_filesystem_fetch_not_found() {
        test::setup();
        let dir = test::tempdir();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };

        let cfg = SharedCacheConfig::Fs(FilesystemSharedCacheConfig {
            path: dir.path().to_path_buf(),
        });
        let svc = SharedCacheService::new(Some(cfg));

        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;

        assert!(!ret);
        assert_eq!(writer, b"");
    }

    #[tokio::test]
    async fn test_filesystem_store() -> Result<()> {
        test::setup();
        let dir = test::tempdir();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Global,
            },
        };
        let cache_path = dir.path().join(key.relative_path());

        let cfg = SharedCacheConfig::Fs(FilesystemSharedCacheConfig {
            path: dir.path().to_path_buf(),
        });
        let svc = SharedCacheService::new(Some(cfg));

        // This mimics how the downloader and Cacher::compute write the cache data.
        let temp_file = NamedTempFile::new_in(&dir)?;
        let dup_file = temp_file.reopen()?;
        let temp_fd = File::from_std(dup_file);
        {
            let mut file = File::create(temp_file.path()).await?;
            file.write_all(b"cache data").await?;
            file.flush().await?;
        }

        svc.store(key, temp_fd).await;

        let data = fs::read(&cache_path).await?;
        assert_eq!(data, b"cache data");
        Ok(())
    }

    #[tokio::test]
    async fn test_gcs_fetch_not_found() -> Result<()> {
        test::setup();
        let credentials = test::gcs_credentials!();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Scoped(Uuid::new_v4().to_string()),
            },
        };

        let cfg = SharedCacheConfig::Gcs(GcsSharedCacheConfig {
            source_key: credentials.source_key(),
            bucket: credentials.bucket,
        });
        let svc = SharedCacheService::new(Some(cfg));

        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;

        assert!(!ret);
        assert_eq!(writer, b"");
        Ok(())
    }

    #[tokio::test]
    async fn test_gcs_state_fetch_not_found() -> Result<()> {
        test::setup();
        let credentials = test::gcs_credentials!();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Scoped(Uuid::new_v4().to_string()),
            },
        };

        let state = GcsState::new(GcsSharedCacheConfig {
            source_key: credentials.source_key(),
            bucket: credentials.bucket,
        });

        let mut writer = Vec::new();

        let ret = state.fetch(&key, &mut writer).await?;

        assert!(ret.is_none());
        assert_eq!(writer, b"");
        Ok(())
    }

    #[tokio::test]
    async fn test_gcs_state_store_fetch() -> Result<()> {
        test::setup();
        let dir = test::tempdir();

        let key = SharedCacheKey {
            name: CacheName::Objects,
            version: 0,
            local_key: CacheKey {
                cache_key: "some_item".to_string(),
                scope: Scope::Scoped(Uuid::new_v4().to_string()),
            },
        };

        let credentials = test::gcs_credentials!();
        let cfg = SharedCacheConfig::Gcs(GcsSharedCacheConfig {
            source_key: credentials.source_key(),
            bucket: credentials.bucket.clone(),
        });
        let svc = SharedCacheService::new(Some(cfg));

        // This mimics how the downloader and Cacher::compute write the cache data.
        let temp_file = NamedTempFile::new_in(&dir)?;
        let dup_file = temp_file.reopen()?;
        let temp_fd = File::from_std(dup_file);
        {
            let mut file = File::create(temp_file.path()).await?;
            file.write_all(b"cache data").await?;
            file.flush().await?;
        }

        svc.store(key.clone(), temp_fd).await;

        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;

        assert!(ret);
        assert_eq!(writer, b"cache data");

        Ok(())
    }
}
