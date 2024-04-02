//! A global cache to be shared between different Symbolicator instances.
//!
//! The goal of this cache is to have a faster warm-up time when starting a new Symbolicator
//! instance by reducing the cost of populating its cache via an additional caching layer that
//! lives closer to Symbolicator. Expensive computations related to the computation of derived
//! caches may also be saved via this shared cache.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Error, Result};
use futures::{Future, TryStreamExt};
use gcp_auth::{AuthenticationManager, CustomServiceAccount, Token};
use reqwest::{Body, Client, StatusCode};
use sentry::protocol::Context;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::ByteView;
use tempfile::NamedTempFile;
use tokio::fs::{self, File};
use tokio::io::{self, AsyncWrite};
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio_util::io::{ReaderStream, StreamReader};
use url::Url;

use crate::download::MeasureSourceDownloadGuard;
use crate::utils::futures::CancelOnDrop;
use crate::utils::gcs::{self, GcsError};

use super::CacheName;

pub mod config;

pub use config::SharedCacheConfig;
use config::{FilesystemSharedCacheConfig, GcsSharedCacheConfig, SharedCacheBackendConfig};

// TODO: get timeouts from global config?
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const STORE_TIMEOUT: Duration = Duration::from_secs(60);

/// Errors using the cache backend.
///
/// This exists since some special cache errors should not be logged since they are
/// considered to be normal at scale, as long as their ratio stays low.
#[derive(thiserror::Error, Debug)]
enum CacheError {
    #[error("timeout connecting to cache service")]
    ConnectTimeout,
    #[error("timeout fetching from cache service")]
    Timeout,
    #[error(transparent)]
    Other(#[from] Error),
}

struct GcsState {
    config: GcsSharedCacheConfig,
    client: Client,
    auth_manager: AuthenticationManager,
}

impl fmt::Debug for GcsState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcsState")
            .field("config", &self.config)
            .field("client", &self.client)
            .field("auth_manager", &"<AuthenticationManager>")
            .finish()
    }
}

pub fn measure_download_time<'a, F, T, E>(
    metric_prefix: &'a str,
    source_name: &'a str,
    f: F,
) -> impl Future<Output = F::Output> + 'a
where
    F: 'a + Future<Output = Result<T, E>>,
{
    let guard = MeasureSourceDownloadGuard::new(metric_prefix, source_name);
    async move {
        let output = f.await;
        guard.done(&output);
        output
    }
}

impl GcsState {
    pub async fn try_new(config: GcsSharedCacheConfig) -> Result<Self> {
        let auth_manager = match config.service_account_path {
            Some(ref path) => {
                let service_account = CustomServiceAccount::from_file(path)?;
                AuthenticationManager::try_from(service_account)?
            }
            None => {
                // For fresh k8s pods the GKE metadata server may not accept connections
                // yet, so we need to retry this for a bit.
                const MAX_DELAY: Duration = Duration::from_secs(60);
                const RETRY_INTERVAL: Duration = Duration::from_millis(500);
                let start = Instant::now();
                loop {
                    let future = async move {
                        AuthenticationManager::new()
                            .await
                            .context("Failed to initialise authentication token")
                    };
                    match tokio::time::timeout(Duration::from_secs(1), future)
                        .await
                        .unwrap_or_else(|_elapsed| {
                            Err(Error::msg("Timeout initialising GCS authentication token"))
                        }) {
                        Ok(auth_manager) => break auth_manager,
                        Err(err) if start.elapsed() > MAX_DELAY => return Err(err),
                        Err(err) => {
                            let remaining = MAX_DELAY - start.elapsed();
                            tracing::warn!("Error initialising GCS authentication token: {}", &err);
                            match err.downcast_ref::<gcp_auth::Error>() {
                                Some(gcp_auth::Error::NoAuthMethod(gcloud, svc, user)) => {
                                    tracing::error!(
                                        "No GCP auth: gcloud: {}, svc: {}, user: {}",
                                        gcloud,
                                        svc,
                                        user,
                                    );
                                }
                                _ => tracing::warn!(
                                    "Error initialising GCS authentication token: {}",
                                    &err
                                ),
                            }
                            tracing::info!(
                                "Waiting for GKE metadata server, {}s remaining",
                                remaining.as_secs(),
                            );
                            tokio::time::sleep(RETRY_INTERVAL).await;
                        }
                    }
                }
            }
        };
        Ok(Self {
            config,
            client: Client::new(),
            auth_manager,
        })
    }

    /// Returns a GCP authentication token, with timeout and error handling.
    ///
    /// Refreshing tokens involves talking to services over networks, this might fail.
    async fn get_token(&self) -> Result<Token> {
        let future = async {
            self.auth_manager
                .get_token(&["https://www.googleapis.com/auth/devstorage.read_write"])
                .await
                .context("Failed to get authentication token")
        };
        tokio::time::timeout(Duration::from_millis(300), future)
            .await
            .unwrap_or_else(|_| Err(Error::msg("Timeout refreshing GCS authentication token")))
    }

    /// Fetches item from shared cache if available and copies them to the writer.
    ///
    /// # Returns
    ///
    /// If successful the number of bytes written to the writer are returned.
    async fn fetch<W>(&self, key: &str, writer: &mut W) -> Result<Option<u64>, CacheError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        // FIXME: This function is pretty much duplicated with `download_reqwest` except for
        // logging, metrics and error handling.
        sentry::configure_scope(|scope| {
            let mut map = BTreeMap::new();
            map.insert("bucket".to_string(), self.config.bucket.clone().into());
            map.insert("key".to_string(), key.into());
            scope.set_context("GCS Shared Cache", Context::Other(map));
        });
        let token = self.get_token().await?;
        let url = gcs::download_url(&self.config.bucket, key).context("URL construction failed")?;
        let request = self.client.get(url).bearer_auth(token.as_str()).send();
        let request = tokio::time::timeout(CONNECT_TIMEOUT, request);
        let request = measure_download_time("services.shared_cache.fetch.connect", "gcs", request);

        let response = request
            .await
            .map_err(|_| CacheError::ConnectTimeout)?
            .map_err(|err| {
                tracing::trace!("Error in shared_cache GCS response for {}", key);
                Error::new(err).context("Bad GCS response for shared_cache")
            })?;

        let status = response.status();
        match status {
            _ if status.is_success() => {
                tracing::trace!("Success hitting shared_cache GCS {}", key);
                let stream = response
                    .bytes_stream()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
                let mut stream = StreamReader::new(stream);

                // NOTE: this is not really a `STORE`, but we use the same timeout here regardless,
                // as we expect this operation to be fast and want to time it out early.
                let future = tokio::time::timeout(STORE_TIMEOUT, io::copy(&mut stream, writer));
                let res = future
                    .await
                    .map_err(|_| CacheError::Timeout)?
                    .context("IO Error streaming HTTP bytes to writer")
                    .map_err(CacheError::Other);

                Some(res).transpose()
            }
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::FORBIDDEN => {
                Err(anyhow!("Insufficient permissions for bucket {}", self.config.bucket).into())
            }
            StatusCode::UNAUTHORIZED => Err(anyhow!("Invalid credentials").into()),
            _ => Err(anyhow!("Error response from GCS: {}", status).into()),
        }
    }

    async fn exists(&self, cache: CacheName, key: &str) -> Result<bool, CacheError> {
        let token = self.get_token().await?;
        let url =
            gcs::object_url(&self.config.bucket, key).context("failed to build object url")?;
        let request = self.client.get(url).bearer_auth(token.as_str()).send();
        let request = tokio::time::timeout(CONNECT_TIMEOUT, request);

        let ret = match request.await {
            Ok(Ok(response)) => {
                // Consume the response body to be nice to the server, it is only a bit of JSON.
                let status = response.status();
                response.bytes().await.ok();

                match status {
                    StatusCode::OK => Ok(true),
                    StatusCode::NOT_FOUND => Ok(false),
                    status => Err(anyhow!("Unexpected status code from GCS: {}", status).into()),
                }
            }
            Ok(Err(err)) => Err(err).context("Error connecting to GCS")?,
            Err(_) => Err(CacheError::ConnectTimeout),
        };
        let status = match ret {
            Ok(_) => "ok",
            Err(CacheError::ConnectTimeout) => "connect-timeout",
            Err(_) => "error",
        };
        metric!(
            counter("services.shared_cache.exists") += 1,
            "cache" => cache.as_ref(),
            "status" => status
        );
        ret
    }

    /// Stores a file on GCS.
    ///
    /// Because we use a very dumb API to upload files we always upload the data over the
    /// network even if the file already exists.  To reduce this, when `reason` is given as
    /// [`CacheStoreReason::Refresh`] this first fetches the metadata to check if the file
    /// exists.  This is racy, but reduces the number of times we spend sending data across
    /// for no reason.
    async fn store(
        &self,
        cache: CacheName,
        key: &str,
        content: ByteView<'static>,
        reason: CacheStoreReason,
    ) -> Result<SharedCacheStoreResult, CacheError> {
        sentry::configure_scope(|scope| {
            let mut map = BTreeMap::new();
            map.insert("bucket".to_string(), self.config.bucket.clone().into());
            map.insert("key".to_string(), key.into());
            scope.set_context("GCS Shared Cache", Context::Other(map));
        });
        if reason == CacheStoreReason::Refresh {
            match self
                .exists(cache, key)
                .await
                .context("Failed fetching GCS object metadata from shared cache")
            {
                Ok(true) => return Ok(SharedCacheStoreResult::Skipped),
                Ok(false) => (),
                Err(err) => match err.downcast_ref::<CacheError>() {
                    Some(CacheError::ConnectTimeout) => (),
                    _ => {
                        sentry::capture_error(&*err);
                    }
                },
            }
        }

        let total_bytes = content.len() as u64;
        let token = self.get_token().await?;
        let mut url =
            Url::parse("https://storage.googleapis.com/upload/storage/v1/b?uploadType=media")
                .map_err(|_| GcsError::InvalidUrl)
                .context("failed to parse url")?;
        // Append path segments manually for proper encoding
        url.path_segments_mut()
            .map_err(|_| GcsError::InvalidUrl)
            .context("failed to build url")?
            .extend(&[&self.config.bucket, "o"]);
        url.query_pairs_mut()
            .append_pair("name", key)
            // Upload only if it's not already there
            .append_pair("ifGenerationMatch", "0");

        let stream = ReaderStream::new(std::io::Cursor::new(content));
        let body = Body::wrap_stream(stream);
        let request = self
            .client
            .post(url.clone())
            .bearer_auth(token.as_str())
            .body(body)
            .send();
        let request = tokio::time::timeout(STORE_TIMEOUT, request);
        let request = measure_download_time("services.shared_cache.store.upload", "gcs", request);

        match request.await {
            Ok(Ok(response)) => {
                let status = response.status();
                match status {
                    successful if successful.is_success() => {
                        tracing::trace!("Success hitting shared_cache GCS {}", key);
                        Ok(SharedCacheStoreResult::Written(total_bytes))
                    }
                    StatusCode::PRECONDITION_FAILED => Ok(SharedCacheStoreResult::Skipped),
                    StatusCode::FORBIDDEN => Err(anyhow!(
                        "Insufficient permissions for bucket {}",
                        self.config.bucket
                    )
                    .into()),
                    StatusCode::UNAUTHORIZED => Err(anyhow!("Invalid credentials").into()),
                    _ => Err(anyhow!("Error response from GCS: {}", status).into()),
                }
            }
            Ok(Err(err)) => {
                tracing::trace!("Error in shared_cache GCS response for {}", key);
                Err(err).context("Bad GCS response for shared_cache")?
            }
            Err(_) => Err(CacheError::ConnectTimeout),
        }
    }
}

impl FilesystemSharedCacheConfig {
    /// Fetches item from shared cache if available and copies them to the writer.
    ///
    /// # Returns
    ///
    /// If successful the number of bytes written to the writer are returned.
    async fn fetch<W>(&self, key: &str, writer: &mut W) -> Result<Option<u64>, CacheError>
    where
        W: AsyncWrite + Unpin,
    {
        let abspath = self.path.join(key);
        tracing::debug!("Fetching debug file from {}", abspath.display());
        let mut file = match File::open(abspath).await {
            Ok(file) => file,
            Err(err) => match err.kind() {
                io::ErrorKind::NotFound => return Ok(None),
                _ => return Err(err).context("Failed to open file in shared cache")?,
            },
        };
        match io::copy(&mut file, writer).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) => Err(err).context("Failed to copy file from shared cache")?,
        }
    }

    async fn store(
        &self,
        // FIXME(swatinem): using a `&str` here leads to a
        // `hidden type ... captures lifetime that does not appear in bounds` error,
        // though not in the GCS uploader for whatever reason
        key: String,
        content: ByteView<'static>,
    ) -> Result<SharedCacheStoreResult, CacheError> {
        let abspath = self.path.join(key);
        let parent_dir = abspath
            .parent()
            .ok_or_else(|| Error::msg("Shared cache directory not found"))?;
        fs::create_dir_all(parent_dir)
            .await
            .context("Failed to create parent directories")?;
        if abspath.as_path().exists() {
            return Ok(SharedCacheStoreResult::Skipped);
        }

        let temp_dir = parent_dir.join(".tmp");
        fs::create_dir_all(&temp_dir)
            .await
            .context("failed to create tempdir")?;
        let temp_file = NamedTempFile::new_in(&temp_dir).context("failed to create tempfile")?;
        let dup_file = temp_file.reopen().context("failed to dup filedescriptor")?;
        let mut dest = File::from_std(dup_file);

        let mut reader: &[u8] = &content;
        let bytes = io::copy(&mut reader, &mut dest)
            .await
            .context("Failed to copy data into file")?;

        temp_file
            .persist(abspath)
            .context("Failed to save file in shared cache")?;
        Ok(SharedCacheStoreResult::Written(bytes))
    }
}

/// The result of an attempt to write an entry to the shared cache.
#[derive(Debug, Clone, Copy)]
enum SharedCacheStoreResult {
    /// Successfully written to the cache as a new entry, contains number of bytes written.
    Written(u64),
    /// Skipped writing the item as it was already on the cache.
    Skipped,
}

impl AsRef<str> for SharedCacheStoreResult {
    fn as_ref(&self) -> &str {
        match self {
            SharedCacheStoreResult::Written(_) => "written",
            SharedCacheStoreResult::Skipped => "skipped",
        }
    }
}

impl fmt::Display for SharedCacheStoreResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum SharedCacheBackend {
    Gcs(Arc<GcsState>),
    Fs(FilesystemSharedCacheConfig),
}

impl SharedCacheBackend {
    /// Creates the backend.
    ///
    /// If the backend can not be created the error will already be reported.
    async fn maybe_new(cfg: SharedCacheBackendConfig) -> Option<Self> {
        match cfg {
            SharedCacheBackendConfig::Gcs(cfg) => {
                match GcsState::try_new(cfg)
                    .await
                    .context("Failed to initialise GCS backend for shared cache")
                {
                    Ok(state) => Some(SharedCacheBackend::Gcs(Arc::new(state))),
                    Err(err) => {
                        sentry::capture_error(&*err);
                        None
                    }
                }
            }
            // TODO: We could check if we can write in the configured directory here, but
            // this is only test backend so not very important.
            SharedCacheBackendConfig::Filesystem(cfg) => Some(SharedCacheBackend::Fs(cfg)),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Gcs(_) => "GCS",
            Self::Fs(_) => "filesystem",
        }
    }
}

/// Message to send upload tasks across the [`SharedCacheService::upload_queue_tx`].
#[derive(Debug)]
struct UploadMessage {
    /// The cache type/name.
    cache: CacheName,
    /// The cache key to store the data at.
    key: String,
    /// The cache contents.
    content: ByteView<'static>,
    /// A channel to notify completion of storage.
    done_tx: oneshot::Sender<()>,
    /// The reason to store this item.
    reason: CacheStoreReason,
}

/// Reasons to store items in the shared cache.
///
/// This is used for reporting metrics only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStoreReason {
    /// The item was newly fetched and never encountered before.
    New,
    /// The item was already found in the local cache, but we extended its lifetime.
    Refresh,
}

impl AsRef<str> for CacheStoreReason {
    fn as_ref(&self) -> &str {
        match self {
            CacheStoreReason::New => "new",
            CacheStoreReason::Refresh => "refresh",
        }
    }
}

pub type SharedCacheRef = Arc<OnceCell<SharedCacheService>>;

/// A shared cache service.
///
/// Initialising is asynchronous since it may take some time.
#[derive(Debug, Clone)]
pub struct SharedCacheService {
    backend: Arc<SharedCacheBackend>,
    upload_queue_tx: mpsc::Sender<UploadMessage>,
    runtime: tokio::runtime::Handle,
}

impl SharedCacheService {
    pub fn new(
        config: Option<SharedCacheConfig>,
        runtime: tokio::runtime::Handle,
    ) -> SharedCacheRef {
        let cache = SharedCacheRef::default();
        if let Some(config) = config {
            runtime.spawn(Self::init(runtime.clone(), cache.clone(), config));
        }
        cache
    }

    async fn init(
        runtime: tokio::runtime::Handle,
        cache: SharedCacheRef,
        config: SharedCacheConfig,
    ) {
        let (tx, rx) = mpsc::channel(config.max_upload_queue_size);
        if let Some(backend) = SharedCacheBackend::maybe_new(config.backend).await {
            let backend = Arc::new(backend);
            tokio::spawn(
                Self::upload_worker(rx, backend.clone(), config.max_concurrent_uploads)
                    .bind_hub(Hub::new_from_top(Hub::current())),
            );
            let _ = cache.set(SharedCacheService {
                backend,
                upload_queue_tx: tx,
                runtime,
            });
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
                    tokio::spawn(
                        Self::single_uploader(done_tx.clone(), backend.clone(), message)
                            .bind_hub(Hub::new_from_top(Hub::current()))
                    );
                    let uploads_in_flight: u64 = (max_concurrent_uploads - uploads_counter) as u64;
                    metric!(gauge("services.shared_cache.uploads_in_flight") = uploads_in_flight);
                }
                Some(_) = done_rx.recv() => {
                    uploads_counter += 1;
                }
                else => break,
            }
        }
        tracing::info!("Shared cache upload worker terminated");
    }

    /// Does a single upload to the shared cache backend.
    ///
    /// Handles metrics and error reporting.
    async fn single_uploader(
        done_tx: mpsc::Sender<()>,
        backend: Arc<SharedCacheBackend>,
        message: UploadMessage,
    ) {
        let UploadMessage {
            cache,
            key,
            content,
            done_tx: complete_tx,
            reason,
        } = message;

        sentry::configure_scope(|scope| {
            let mut map = BTreeMap::new();
            map.insert("backend".to_string(), backend.name().into());
            map.insert("cache".to_string(), cache.as_ref().into());
            map.insert("path".to_string(), key.clone().into());
            scope.set_context("Shared Cache", Context::Other(map));
        });

        let res = match *backend {
            SharedCacheBackend::Gcs(ref state) => state.store(cache, &key, content, reason).await,
            SharedCacheBackend::Fs(ref cfg) => cfg.store(key, content).await,
        };
        match res {
            Ok(op) => {
                metric!(
                    counter("services.shared_cache.store") += 1,
                    "cache" => cache.as_ref(),
                    "write" => op.as_ref(),
                    "status" => "ok",
                    "reason" => reason.as_ref(),
                );
                if let SharedCacheStoreResult::Written(bytes) = op {
                    let bytes: i64 = bytes.try_into().unwrap_or(i64::MAX);
                    metric!(
                        counter("services.shared_cache.store.bytes") += bytes,
                        "cache" => cache.as_ref(),
                    );
                }
            }
            Err(outer_err) => {
                let errdetails = match outer_err {
                    CacheError::ConnectTimeout => "connect-timeout",
                    CacheError::Timeout => "timeout",
                    CacheError::Other(_) => "other",
                };
                if let CacheError::Other(err) = outer_err {
                    let stderr: &dyn std::error::Error = &*err;
                    tracing::error!(
                        stderr,
                        "Error storing file on {} shared cache",
                        backend.name(),
                    );
                }
                metric!(
                    counter("services.shared_cache.store") += 1,
                    "cache" => cache.as_ref(),
                    "status" => "error",
                    "reason" => reason.as_ref(),
                    "errdetails" => errdetails,
                );
            }
        }

        // Tell the work coordinator we're done.
        done_tx.send(()).await.unwrap_or_else(|err| {
            let stderr: &dyn std::error::Error = &err;
            tracing::error!(
                stderr,
                "Shared cache single_uploader failed to send done message",
            );
        });

        // Tell the original work submitter we're done, if they dropped this we don't care.
        complete_tx.send(()).ok();
    }

    /// Returns the name of the backend configured.
    fn backend_name(&self) -> &'static str {
        self.backend.name()
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
    #[tracing::instrument(name = "fetch_shared_cache", skip(self, file))]
    pub async fn fetch(&self, cache: CacheName, key: &str, mut file: tokio::fs::File) -> bool {
        let _guard = Hub::current().push_scope();
        let backend_name = self.backend_name();
        let key = format!("{}/{key}", cache.as_ref());
        sentry::configure_scope(|scope| {
            let mut map = BTreeMap::new();
            map.insert("backend".to_string(), backend_name.into());
            map.insert("cache".to_string(), cache.as_ref().into());
            map.insert("path".to_string(), key.clone().into());
            scope.set_context("Shared Cache", Context::Other(map));
        });
        let res = match self.backend.as_ref() {
            SharedCacheBackend::Gcs(state) => {
                let state = Arc::clone(state);
                let future = async move { state.fetch(&key, &mut file).await }
                    .bind_hub(sentry::Hub::current());
                let future = CancelOnDrop::new(self.runtime.spawn(future));
                let future = tokio::time::timeout(STORE_TIMEOUT, future);
                future
                    .await
                    .map(|res| res.unwrap_or(Err(CacheError::ConnectTimeout)))
                    .unwrap_or(Err(CacheError::Timeout))
            }
            SharedCacheBackend::Fs(cfg) => cfg.fetch(&key, &mut file).await,
        };
        match res {
            Ok(Some(bytes)) => {
                metric!(
                    counter("services.shared_cache.fetch") += 1,
                    "cache" => cache.as_ref(),
                    "hit" => "true",
                    "status" => "ok",
                );
                let bytes: i64 = bytes.try_into().unwrap_or(i64::MAX);
                metric!(
                    counter("services.shared_cache.fetch.bytes") += bytes,
                    "cache" => cache.as_ref(),
                );
                true
            }
            Ok(None) => {
                metric!(
                    counter("services.shared_cache.fetch") += 1,
                    "cache" => cache.as_ref(),
                    "hit" => "false",
                    "status" => "ok",
                );
                false
            }
            Err(outer_err) => {
                let errdetails = match outer_err {
                    CacheError::ConnectTimeout => "connect-timeout",
                    CacheError::Timeout => "timeout",
                    CacheError::Other(_) => "other",
                };
                if let CacheError::Other(err) = outer_err {
                    let stderr: &dyn std::error::Error = &*err;
                    tracing::error!(stderr, "Error fetching from {} shared cache", backend_name);
                }
                metric!(
                    counter("services.shared_cache.fetch") += 1,
                    "cache" => cache.as_ref(),
                    "status" => "error",
                    "errdetails" => errdetails,
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
    /// A [`oneshot::Receiver`] is returned which will
    /// receive a value once the file has been stored in the shared cache.  Due to
    /// backpressure it is possible that the file is never stored, in which case the
    /// corresponding [`oneshot::Sender`] is dropped and awaiting the receiver will resolve
    /// into an [`Err`].
    ///
    /// This [`oneshot::Receiver`] can also be safely ignored if you do not need to know
    /// when the file is stored.  This mostly exists to enable testing.
    ///
    /// If [`CacheStoreReason::Refresh`] is used the implementation will trade off an extra
    /// request to check if the file already exists before uploading.  This is racy but a
    /// good tradeoff for refreshed stores.
    pub fn store(
        &self,
        cache: CacheName,
        key: &str,
        content: ByteView<'static>,
        reason: CacheStoreReason,
    ) -> oneshot::Receiver<()> {
        let (done_tx, done_rx) = oneshot::channel::<()>();
        // We want to throttle refreshes to a lower number, as refreshes happen every ~1h if a cache
        // item is actively being used. That is quite some overhead considering that we only refresh
        // an item once it expires.
        if reason == CacheStoreReason::Refresh && rand::random::<f32>() > 0.05 {
            metric!(counter("services.shared_cache.store.discarded") += 1);
            tracing::debug!("Randomly discarded shared cache refresh");
            let _ = done_tx.send(());
            return done_rx;
        }
        metric!(
            gauge("services.shared_cache.uploads_queue_capacity") =
                self.upload_queue_tx.capacity() as u64
        );
        self.upload_queue_tx
            .try_send(UploadMessage {
                cache,
                key: format!("{}/{key}", cache.as_ref()),
                content,
                done_tx,
                reason,
            })
            .unwrap_or_else(|_| {
                metric!(counter("services.shared_cache.store.dropped") += 1);
                tracing::error!("Shared cache upload queue full");
            });
        done_rx
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use symbolicator_test::TestGcsCredentials;

    use super::*;

    impl From<TestGcsCredentials> for GcsSharedCacheConfig {
        fn from(source: TestGcsCredentials) -> Self {
            Self {
                bucket: source.bucket,
                service_account_path: source.credentials_file,
            }
        }
    }

    async fn wait_init(service: &SharedCacheRef) -> &SharedCacheService {
        const MAX_DELAY: Duration = Duration::from_secs(3);
        let start = Instant::now();
        loop {
            if start.elapsed() > MAX_DELAY {
                panic!("shared cache not ready");
            }
            if let Some(service) = service.get() {
                return service;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn test_filesystem_fetch_found() {
        symbolicator_test::setup();
        let dir = symbolicator_test::tempdir();

        let key = "global/some_item";
        let cache_path = dir.path().join("objects/global/some_item");
        fs::create_dir_all(cache_path.parent().unwrap())
            .await
            .unwrap();
        fs::write(&cache_path, b"cache data").await.unwrap();

        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Filesystem(FilesystemSharedCacheConfig {
                path: dir.path().to_path_buf(),
            }),
        };
        let svc = SharedCacheService::new(Some(cfg), tokio::runtime::Handle::current());
        let svc = wait_init(&svc).await;

        // This mimics how Cacher::compute creates this file.
        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let file = File::from_std(temp_file.reopen().unwrap());

        let ret = svc.fetch(CacheName::Objects, key, file).await;
        assert!(ret);

        let buf = std::fs::read(temp_file).unwrap();
        assert_eq!(buf, b"cache data");
    }

    #[tokio::test]
    async fn test_filesystem_fetch_not_found() {
        symbolicator_test::setup();
        let dir = symbolicator_test::tempdir();

        let key = "global/some_item";

        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Filesystem(FilesystemSharedCacheConfig {
                path: dir.path().to_path_buf(),
            }),
        };
        let svc = SharedCacheService::new(Some(cfg), tokio::runtime::Handle::current());
        let svc = wait_init(&svc).await;

        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let file = File::from_std(temp_file.reopen().unwrap());

        let ret = svc.fetch(CacheName::Objects, key, file).await;
        assert!(!ret);

        let buf = std::fs::read(temp_file).unwrap();
        assert_eq!(buf, b"");
    }

    #[tokio::test]
    async fn test_filesystem_store() {
        symbolicator_test::setup();
        let dir = symbolicator_test::tempdir();

        let key = "global/some_item";

        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Filesystem(FilesystemSharedCacheConfig {
                path: dir.path().to_path_buf(),
            }),
        };
        let svc = SharedCacheService::new(Some(cfg), tokio::runtime::Handle::current());
        let svc = wait_init(&svc).await;

        let recv = svc.store(
            CacheName::Objects,
            key,
            ByteView::from_slice(b"cache data"),
            CacheStoreReason::New,
        );
        // Wait for storing to complete.
        recv.await.unwrap();

        let cache_path = dir.path().join("objects/global/some_item");
        let data = fs::read(&cache_path)
            .await
            .context("Failed to read written cache file")
            .unwrap();
        assert_eq!(data, b"cache data");
    }

    #[tokio::test]
    async fn test_gcs_fetch_not_found() {
        symbolicator_test::setup();
        let credentials = symbolicator_test::gcs_credentials!();

        let key = format!("0/{}/some_item", Uuid::new_v4());

        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Gcs(GcsSharedCacheConfig::from(credentials)),
        };
        let svc = SharedCacheService::new(Some(cfg), tokio::runtime::Handle::current());
        let svc = wait_init(&svc).await;

        let dir = symbolicator_test::tempdir();
        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let file = File::from_std(temp_file.reopen().unwrap());

        let ret = svc.fetch(CacheName::Objects, &key, file).await;
        assert!(!ret);

        let buf = std::fs::read(temp_file).unwrap();
        assert_eq!(buf, b"");
    }

    #[tokio::test]
    async fn test_gcs_state_fetch_not_found() {
        symbolicator_test::setup();
        let credentials = symbolicator_test::gcs_credentials!();

        let key = format!("{}/some_item", Uuid::new_v4());

        let state = GcsState::try_new(GcsSharedCacheConfig::from(credentials))
            .await
            .unwrap();

        let mut writer = Vec::new();

        let ret = state.fetch(&key, &mut writer).await.unwrap();

        assert!(ret.is_none());
        assert_eq!(writer, b"");
    }

    #[tokio::test]
    async fn test_gcs_svc_store_fetch() {
        symbolicator_test::setup();
        let dir = symbolicator_test::tempdir();

        let key = format!("{}/some_item", Uuid::new_v4());

        let credentials = symbolicator_test::gcs_credentials!();
        let cfg = SharedCacheConfig {
            max_concurrent_uploads: 10,
            max_upload_queue_size: 10,
            backend: SharedCacheBackendConfig::Gcs(GcsSharedCacheConfig::from(credentials)),
        };
        let svc = SharedCacheService::new(Some(cfg), tokio::runtime::Handle::current());
        let svc = wait_init(&svc).await;

        let recv = svc.store(
            CacheName::Objects,
            &key,
            ByteView::from_slice(b"cache data"),
            CacheStoreReason::New,
        );
        // Wait for storing to complete.
        recv.await.unwrap();

        let temp_file = NamedTempFile::new_in(&dir).unwrap();
        let file = File::from_std(temp_file.reopen().unwrap());

        let ret = svc.fetch(CacheName::Objects, &key, file).await;
        assert!(ret);

        let buf = std::fs::read(temp_file).unwrap();
        assert_eq!(buf, b"cache data");
    }

    #[tokio::test]
    async fn test_gcs_state_store_twice() {
        symbolicator_test::setup();
        let credentials = symbolicator_test::gcs_credentials!();

        let key = format!("{}/some_item", Uuid::new_v4());

        let state = GcsState::try_new(GcsSharedCacheConfig::from(credentials))
            .await
            .unwrap();

        let ret = state
            .store(
                CacheName::Objects,
                &key,
                ByteView::from_slice(b"cache data"),
                CacheStoreReason::New,
            )
            .await
            .unwrap();

        assert!(matches!(ret, SharedCacheStoreResult::Written(_)));

        let ret = state
            .store(
                CacheName::Objects,
                &key,
                ByteView::from_slice(b"cache data"),
                CacheStoreReason::New,
            )
            .await
            .unwrap();

        assert!(matches!(ret, SharedCacheStoreResult::Skipped));
    }

    #[tokio::test]
    async fn test_gcs_exists() {
        symbolicator_test::setup();
        let credentials = symbolicator_test::gcs_credentials!();
        let state = GcsState::try_new(GcsSharedCacheConfig::from(credentials))
            .await
            .unwrap();

        let key = format!("{}/some_item", Uuid::new_v4());

        assert!(!state.exists(CacheName::Objects, &key).await.unwrap());

        state
            .store(
                CacheName::Objects,
                &key,
                ByteView::from_slice(b"cache data"),
                CacheStoreReason::New,
            )
            .await
            .unwrap();

        assert!(state.exists(CacheName::Objects, &key).await.unwrap());
    }
}
