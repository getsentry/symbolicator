//! A global cache to be shared between different symbolicator instances.
//!
//! The goal of this cache is to have a faster warmup time when starting a new symbolicator
//! instance.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Error, Result};
use futures::TryStreamExt;
use reqwest::{Body, Client, StatusCode};
use tokio::fs::{self, File};
use tokio::io::{self, AsyncSeekExt, AsyncWrite};
use tokio::sync::{mpsc, oneshot, RwLock};
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

        // TODO: this must be atomic!  can't have partially written files in the cache.
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

impl SharedCacheBackend {
    fn name(&self) -> &'static str {
        match self {
            Self::Gcs(_) => "GCS",
            Self::Fs(_) => "filesystem",
        }
    }
}

/// Message to send upload tasks across the [`SharedCacheService::upload_queue_tx`].
struct UploadMessage {
    /// The cache key to store the data at.
    key: SharedCacheKey,
    /// The [`File`] to read the cache data from.
    src: File,
    /// A channel to notify completion of storage.
    done_tx: oneshot::Sender<()>,
}

/// A shared cache service.
///
/// For simplicity in the rest of the application this service always exists, regardless of
/// whether it is configured or not.  If it is not configured calls to it's methods become a
/// no-op.
#[derive(Debug)]
pub struct SharedCacheService {
    backend: Option<Arc<SharedCacheBackend>>,
    upload_queue_tx: Option<mpsc::Sender<UploadMessage>>,
}

impl SharedCacheService {
    pub fn new(config: Option<SharedCacheConfig>) -> Self {
        // TODO: move these to the config
        let max_queue_size = 50;
        let max_concurrent_uploads = 25;

        match config {
            Some(cfg) => {
                let (tx, rx) = mpsc::channel(max_queue_size);
                let backend = match cfg {
                    SharedCacheConfig::Gcs(cfg) => SharedCacheBackend::Gcs(GcsState::new(cfg)),
                    SharedCacheConfig::Fs(cfg) => SharedCacheBackend::Fs(cfg),
                };
                let backend = Arc::new(backend);
                tokio::spawn(Self::upload_worker(
                    rx,
                    backend.clone(),
                    max_concurrent_uploads,
                ));
                Self {
                    backend: Some(backend),
                    upload_queue_tx: Some(tx),
                }
            }
            None => Self {
                backend: None,
                upload_queue_tx: None,
            },
        }
    }

    /// Long running task managing concurrent uploads to the shared cache.
    async fn upload_worker(
        mut work_rx: mpsc::Receiver<UploadMessage>,
        backend: Arc<SharedCacheBackend>,
        max_concurrent_uploads: usize,
    ) {
        let (done_tx, mut done_rx) = mpsc::channel::<()>(max_concurrent_uploads);
        let mut uploads_counter = max_concurrent_uploads;
        loop {
            tokio::select! {
                Some(message) = work_rx.recv(), if uploads_counter > 0 => {
                    uploads_counter -= 1;
                    tokio::spawn(Self::single_uploader(done_tx.clone(), backend.clone(), message));
                    let uploads_in_flight: u64 = (max_concurrent_uploads - uploads_counter) as u64;
                    metric!(gauge("services.shared_cache.uploads_in_flight") = uploads_in_flight);
                }
                Some(_) = done_rx.recv() => {
                    uploads_counter += 1;
                }
                else => break,
            }
        }
        log::info!("Shared cache upload worker terminated");
    }

    /// Does a single upload to the shared cache backend.
    ///
    /// Handles metrics and error reporting.
    async fn single_uploader(
        done_tx: mpsc::Sender<()>,
        backend: Arc<SharedCacheBackend>,
        message: UploadMessage,
    ) {
        let UploadMessage { key, src, done_tx: complete_tx } = message;
        let cache_name = key.name;
        let res = match *backend {
            SharedCacheBackend::Gcs(ref state) => state.store(key, src).await,
            SharedCacheBackend::Fs(ref cfg) => cfg.store(key, src).await,
        };
        match res {
            Ok(op) => {
                metric!(
                    counter("services.shared_cache.store") += 1,
                    "cache" => cache_name.as_ref(),
                    "write" => op.as_ref(),
                );
            }
            Err(err) => {
                log::error!(
                    "Error storing file on {} shared cache: {}",
                    backend.name(),
                    LogError(&*err),
                );
            }
        }

        // Tell the work coordinator we're done.
        done_tx.send(()).await.unwrap_or_else(|err| {
            log::error!(
                "Shared cache single_uploader failed to send done message: {}",
                LogError(&err)
            );
        });

        // Tell the original work submitter we're done, if they dropped this we don't care.
        complete_tx.send(()).ok();
    }

    /// Returns the name of the backend configured.
    fn backend_name(&self) -> &'static str {
        match self.backend {
            Some(ref backend) => backend.name(),
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
        let res = match self.backend.as_deref() {
            Some(backend) => match backend {
                SharedCacheBackend::Gcs(ref state) => state.fetch(key, writer).await,
                SharedCacheBackend::Fs(ref cfg) => cfg.fetch(key, writer).await,
            },
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
    ///
    /// # Return
    ///
    /// If the shared cache is enabled a [`oneshot::Receiver`] is returned which will
    /// receive a value once the file has been stored in the shared cache.  Due to
    /// backpressure it is possible that the file is never stored, in which case the
    /// corresponding [`oneshot::Sender`] is dropped and awaiting the receiver will resolve
    /// into an [`Err`].
    ///
    /// This [`oneshot::Receiver`] can also be safely ignored if you do not need to know
    /// when the file is stored.  This mostly exists to enable testing.
    pub async fn store(&self, key: SharedCacheKey, src: File) -> Option<oneshot::Receiver<()>> {
        match self.upload_queue_tx {
            Some(ref work_tx) => {
                metric!(
                    gauge("services.shared_cache.uploads_queue_capacity") =
                        work_tx.capacity() as u64
                );
                let (done_tx, done_rx) = oneshot::channel::<()>();
                work_tx
                    .try_send(UploadMessage { key, src, done_tx })
                    .unwrap_or_else(|_| {
                        metric!(counter("services.shared_cache.store.dropped") += 1);
                        log::error!("Shared cache upload queue full");
                    });
                Some(done_rx)
            }
            None => None,
        }
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
    async fn test_filesystem_fetch_found() {
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
        fs::create_dir_all(cache_path.parent().unwrap()).await.unwrap();
        fs::write(&cache_path, b"cache data").await.unwrap();

        let cfg = SharedCacheConfig::Fs(FilesystemSharedCacheConfig {
            path: dir.path().to_path_buf(),
        });
        let svc = SharedCacheService::new(Some(cfg));

        // This mimics how Cacher::compute creates this file.
        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let stdfile = temp_file.reopen().unwrap();
        let mut file = File::from_std(stdfile);

        let ret = svc.fetch(&key, &mut file).await;

        assert!(ret);
        file.rewind().await.unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"cache data");
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
    async fn test_filesystem_store() {
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
        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let dup_file = temp_file.reopen().unwrap();
        let temp_fd = File::from_std(dup_file);
        {
            let mut file = File::create(temp_file.path()).await.unwrap();
            file.write_all(b"cache data").await.unwrap();
            file.flush().await.unwrap();
        }

        if let Some(recv) = svc.store(key, temp_fd).await {
            // Wait for storing to complete.
            recv.await.unwrap();
        }

        let data = fs::read(&cache_path)
            .await
            .context("Failed to read written cache file")
            .unwrap();
        assert_eq!(data, b"cache data");
    }

    #[tokio::test]
    async fn test_gcs_fetch_not_found() {
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
    }

    #[tokio::test]
    async fn test_gcs_state_fetch_not_found() {
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

        let ret = state.fetch(&key, &mut writer).await.unwrap();

        assert!(ret.is_none());
        assert_eq!(writer, b"");
    }

    #[tokio::test]
    async fn test_gcs_svc_store_fetch() {
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
        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let dup_file = temp_file.reopen().unwrap();
        let temp_fd = File::from_std(dup_file);
        {
            let mut file = File::create(temp_file.path()).await.unwrap();
            file.write_all(b"cache data").await.unwrap();
            file.flush().await.unwrap();
        }

        if let Some(recv) = svc.store(key.clone(), temp_fd).await {
            // Wait for storing to complete.
            recv.await.unwrap();
        }

        let mut writer = Vec::new();

        let ret = svc.fetch(&key, &mut writer).await;

        assert!(ret);
        assert_eq!(writer, b"cache data");
    }

    #[tokio::test]
    async fn test_gcs_state_store_twice() {
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

        // This mimics how the downloader and Cacher::compute write the cache data.
        let temp_file = NamedTempFile::new().unwrap();
        let dup_file = temp_file.reopen().unwrap();
        let temp_fd = File::from_std(dup_file);
        {
            let mut file = File::create(temp_file.path()).await.unwrap();
            file.write_all(b"cache data").await.unwrap();
            file.flush().await.unwrap();
        }

        let ret = state.store(key.clone(), temp_fd).await.unwrap();

        assert!(matches!(ret, SharedCacheStoreResult::Written));

        let dup_file = temp_file.reopen().unwrap();
        let temp_fd = File::from_std(dup_file);

        let ret = state.store(key, temp_fd).await.unwrap();

        assert!(matches!(ret, SharedCacheStoreResult::Skipped));
    }
}
