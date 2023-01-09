//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! <https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/>

use std::convert::TryInto;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::sentry::SentryFutureExt;
use futures::prelude::*;
use reqwest::StatusCode;
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

pub use symbolicator_sources::{
    DirectoryLayout, FileType, ObjectId, ObjectType, RemoteFile, RemoteFileUri, SourceConfig,
    SourceFilters, SourceLocation,
};
use symbolicator_sources::{
    FilesystemRemoteFile, GcsRemoteFile, HttpRemoteFile, S3RemoteFile, SourceLocationIter,
};

use crate::cache::CacheError;
use crate::config::{CacheConfigs, Config, InMemoryCacheConfig};
use crate::utils::futures::{self as future_utils, m, measure, CancelOnDrop};
use crate::utils::sentry::ConfigureScope;

mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

impl ConfigureScope for RemoteFile {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        scope.set_tag("source.id", self.source_id());
        scope.set_tag("source.type", self.source_type_name());
        scope.set_tag("source.is_public", self.is_public());
        scope.set_tag("source.uri", self.uri());
    }
}

/// HTTP User-Agent string to use.
const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

/// Errors happening while downloading from sources.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("failed to perform an IO operation")]
    Io(#[source] std::io::Error),
    #[error("failed to stream file")]
    Reqwest(#[source] reqwest::Error),
    #[error("bad file destination")]
    BadDestination(#[source] std::io::Error),
    #[error("failed writing the downloaded file")]
    Write(#[source] std::io::Error),
    #[error("download was cancelled")]
    Canceled,
    #[error("failed to fetch data from GCS")]
    Gcs(#[from] crate::utils::gcs::GcsError),
    #[error("failed to fetch data from Sentry")]
    Sentry(sentry::SentryError),
    #[error("failed to fetch data from S3")]
    S3(#[from] s3::S3Error),
    #[error("S3 error code: {1} (http status: {0})")]
    S3WithCode(StatusCode, String),
    #[error("missing permissions for file")]
    Permissions,
    /// Typically means the initial HEAD request received a non-200, non-400 response.
    #[error("failed to download: {0}")]
    Rejected(StatusCode),
    #[error("failed to fetch object: {0}")]
    CachedError(String),
}

impl From<sentry::SentryError> for DownloadError {
    fn from(err: sentry::SentryError) -> Self {
        if matches!(
            err,
            sentry::SentryError::BadStatusCode(StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED)
        ) {
            DownloadError::Permissions
        } else {
            DownloadError::Sentry(err)
        }
    }
}

impl From<DownloadError> for CacheError {
    fn from(error: DownloadError) -> Self {
        match error {
            DownloadError::Reqwest(e) => {
                let mut innermost: &dyn Error = &e;
                while let Some(src) = innermost.source() {
                    innermost = src;
                }

                let mut error_string = innermost.to_string();

                // Special-case a few error strings
                if error_string.contains("certificate verify failed") {
                    error_string = "certificate verify failed".to_string();
                }

                if error_string.contains("SSL routines") {
                    error_string = "SSL error".to_string();
                }

                Self::DownloadError(error_string)
            }
            DownloadError::Canceled => Self::Timeout(Duration::default()),
            DownloadError::Gcs(e) => {
                Self::DownloadError(format!("failed to fetch data from GCS: {e}"))
            }
            DownloadError::Sentry(e) => {
                Self::DownloadError(format!("failed to fetch data from Sentry: {e}"))
            }
            DownloadError::S3(e) => {
                Self::DownloadError(format!("failed to fetch data from S3: {e}"))
            }
            DownloadError::S3WithCode(status, code) => {
                Self::DownloadError(format!("S3 error code: {code} (http status: {status})"))
            }
            DownloadError::Permissions => Self::PermissionDenied(String::new()),
            DownloadError::Rejected(status_code) => Self::DownloadError(status_code.to_string()),
            _ => Self::from_std_error(error),
        }
    }
}

/// Completion status of a successful download request.
#[derive(Debug)]
pub enum DownloadStatus<T> {
    /// The download completed successfully and the file at the path can be used.
    Completed(T),
    /// The requested file was not found.
    NotFound,
    /// Not enough permissions to download the file.
    PermissionDenied,
}

/// A service which can download files from a [`SourceConfig`].
///
/// The service is rather simple on the outside but will one day control
/// rate limits and the concurrency it uses.
#[derive(Debug)]
pub struct DownloadService {
    runtime: tokio::runtime::Handle,
    max_download_timeout: Duration,
    sentry: sentry::SentryDownloader,
    http: http::HttpDownloader,
    s3: s3::S3Downloader,
    gcs: gcs::GcsDownloader,
    fs: filesystem::FilesystemDownloader,
}

impl DownloadService {
    /// Creates a new downloader that runs all downloads in the given remote thread.
    pub fn new(config: &Config, runtime: tokio::runtime::Handle) -> Arc<Self> {
        let trusted_client = crate::utils::http::create_client(config, true);
        let restricted_client = crate::utils::http::create_client(config, false);

        let Config {
            connect_timeout,
            streaming_timeout,
            caches: CacheConfigs { ref in_memory, .. },
            ..
        } = *config;

        let InMemoryCacheConfig {
            gcs_token_capacity,
            s3_client_capacity,
            ..
        } = in_memory;

        Arc::new(Self {
            runtime: runtime.clone(),
            max_download_timeout: config.max_download_timeout,
            sentry: sentry::SentryDownloader::new(trusted_client, runtime, config),
            http: http::HttpDownloader::new(
                restricted_client.clone(),
                connect_timeout,
                streaming_timeout,
            ),
            s3: s3::S3Downloader::new(connect_timeout, streaming_timeout, *s3_client_capacity),
            gcs: gcs::GcsDownloader::new(
                restricted_client,
                connect_timeout,
                streaming_timeout,
                *gcs_token_capacity,
            ),
            fs: filesystem::FilesystemDownloader::new(),
        })
    }

    /// Dispatches downloading of the given file to the appropriate source.
    async fn dispatch_download(
        &self,
        source: &RemoteFile,
        temp_file: NamedTempFile,
    ) -> Result<DownloadStatus<NamedTempFile>, DownloadError> {
        let destination = temp_file.path();

        let result = future_utils::retry(|| async {
            match source {
                RemoteFile::Sentry(inner) => {
                    self.sentry
                        .download_source(inner.clone(), destination)
                        .await
                }
                RemoteFile::Http(inner) => {
                    self.http.download_source(inner.clone(), destination).await
                }
                RemoteFile::S3(inner) => self.s3.download_source(inner.clone(), destination).await,
                RemoteFile::Gcs(inner) => {
                    self.gcs.download_source(inner.clone(), destination).await
                }
                RemoteFile::Filesystem(inner) => {
                    self.fs.download_source(inner.clone(), destination).await
                }
            }
        });

        match result.await {
            Ok(DownloadStatus::Completed(_)) => {
                tracing::debug!("Fetched debug file from {}", source);
                Ok(DownloadStatus::Completed(temp_file))
            }
            Ok(DownloadStatus::NotFound) => {
                tracing::debug!("Debug file not found at {}", source);
                Ok(DownloadStatus::NotFound)
            }
            Ok(DownloadStatus::PermissionDenied) => {
                tracing::debug!("No permissions to fetch file from {}", source);
                // FIXME: downstream users still expect these to be errors
                Err(DownloadError::Permissions)
            }
            Err(err) => {
                tracing::debug!("Failed to fetch debug file from {}: {}", source, err);
                Err(err)
            }
        }
    }

    /// Download a file from a source and store it on the local filesystem.
    ///
    /// This does not do any deduplication of requests, every requested file is freshly downloaded.
    ///
    /// The downloaded file is saved into `destination`. The file will be created if it does not
    /// exist and truncated if it does. In case of any error, the file's contents is considered
    /// garbage.
    pub async fn download(
        self: Arc<Self>,
        source: RemoteFile,
        destination: NamedTempFile,
    ) -> Result<DownloadStatus<NamedTempFile>, DownloadError> {
        let slf = self.clone();
        let job = async move { slf.dispatch_download(&source, destination).await };
        let job = CancelOnDrop::new(self.runtime.spawn(job.bind_hub(::sentry::Hub::current())));
        let job = tokio::time::timeout(self.max_download_timeout, job);
        let job = measure("service.download", m::timed_result, None, job);

        match job.await {
            Ok(Ok(result)) => result,
            _ => Err(DownloadError::Canceled),
        }
    }

    /// Returns all objects matching the [`ObjectId`] at the source.
    ///
    /// Some sources, namely all the symbol servers, simply return the locations at which a
    /// download attempt should be made without any guarantee the object is actually there.
    ///
    /// If the source needs to be contacted to get matching objects this may fail and
    /// returns a [`DownloadError`].
    ///
    /// Note that the `filetypes` argument is not more then a hint, not all source types
    /// will respect this and they may return all DIFs matching the `object_id`.  After
    /// downloading you may still need to filter the files.
    pub async fn list_files(
        &self,
        sources: &[SourceConfig],
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> Vec<RemoteFile> {
        let mut remote_files = vec![];

        macro_rules! check_source {
            ($source:ident => $file_ty:ty) => {{
                let mut iter =
                    SourceLocationIter::new(&$source.files, filetypes, object_id).peekable();
                if iter.peek().is_none() {
                    // TODO: create a special "no file on source" `RemoteFile`?
                } else {
                    remote_files
                        .extend(iter.map(|loc| <$file_ty>::new($source.clone(), loc).into()))
                }
            }};
        }

        for source in sources {
            match source {
                SourceConfig::Sentry(cfg) => {
                    let job = self.sentry.list_files(cfg.clone(), object_id, filetypes);
                    let job = tokio::time::timeout(Duration::from_secs(30), job);
                    let job = measure("service.download.list_files", m::timed_result, None, job);

                    let sentry_files = job.await.map_err(|_| DownloadError::Canceled);
                    match sentry_files {
                        Ok(Ok(files)) => remote_files.extend(files),
                        Ok(Err(error)) | Err(error) => {
                            let error: &dyn std::error::Error = &error;
                            tracing::error!(error, "Failed to fetch file list");
                            // TODO: create a special "finding files failed" `RemoteFile`?
                        }
                    }
                }
                SourceConfig::Http(cfg) => check_source!(cfg => HttpRemoteFile),
                SourceConfig::S3(cfg) => check_source!(cfg => S3RemoteFile),
                SourceConfig::Gcs(cfg) => check_source!(cfg => GcsRemoteFile),
                SourceConfig::Filesystem(cfg) => check_source!(cfg => FilesystemRemoteFile),
            }
        }
        remote_files
    }
}

/// Download the source from a stream.
///
/// This is common functionality used by many downloaders.
///
/// # Errors
/// - [`DownloadError::BadDestination`]
/// - [`DownloadError::Write`]
/// - [`DownloadError::Canceled`]
async fn download_stream(
    source: &RemoteFile,
    stream: impl Stream<Item = Result<impl AsRef<[u8]>, DownloadError>>,
    destination: &Path,
    timeout: Option<Duration>,
) -> Result<DownloadStatus<()>, DownloadError> {
    // All file I/O in this function is blocking!
    tracing::trace!("Downloading from {}", source);
    let future = async {
        let mut file = File::create(destination)
            .await
            .map_err(DownloadError::BadDestination)?;
        futures::pin_mut!(stream);

        let mut throughput_recorder =
            MeasureSourceDownloadGuard::new("source.download.stream", source.source_metric_key());
        let result: Result<_, DownloadError> = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                let chunk = chunk.as_ref();
                throughput_recorder.add_bytes_transferred(chunk.len() as u64);
                file.write_all(chunk).await.map_err(DownloadError::Write)?;
            }
            Ok(())
        }
        .await;
        throughput_recorder.done(&result);
        result?;

        file.flush().await.map_err(DownloadError::Write)?;
        Ok(DownloadStatus::Completed(()))
    };

    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| DownloadError::Canceled)?,
        None => future.await,
    }
}

/// State of the [`MeasureSourceDownloadGuard`].
#[derive(Clone, Copy, Debug)]
enum MeasureState {
    /// The future is not ready.
    Pending,
    /// The future has terminated with a status.
    Done(&'static str),
}

/// A guard to [`measure`] the amount of time it takes to download a source. This guard is also
/// capable of calculating and reporting the throughput of the connection. Two metrics are
/// emitted if `bytes_transferred` is set:
///
/// 1. Amount of time taken to complete the measurement
/// 2. Connection thoroughput (bytes transferred / time taken to complete)
///
/// If `bytes_transferred` is not set, then only the first metric (amount of time taken) is
/// recorded.
pub struct MeasureSourceDownloadGuard<'a> {
    state: MeasureState,
    task_name: &'a str,
    source_name: &'a str,
    creation_time: Instant,
    bytes_transferred: Option<u64>,
}

impl<'a> MeasureSourceDownloadGuard<'a> {
    /// Creates a new measure guard for downloading a source.
    pub fn new(task_name: &'a str, source_name: &'a str) -> Self {
        Self {
            state: MeasureState::Pending,
            task_name,
            source_name,
            bytes_transferred: None,
            creation_time: Instant::now(),
        }
    }

    /// A checked add to the amount of bytes transferred during the download.
    ///
    /// This value will be emitted when the download's future is completed or cancelled.
    pub fn add_bytes_transferred(&mut self, additional_bytes: u64) {
        let bytes = self.bytes_transferred.get_or_insert(0);
        *bytes = bytes.saturating_add(additional_bytes);
    }

    /// Marks the download as terminated.
    pub fn done<T, E>(mut self, reason: &Result<T, E>) {
        self.state = MeasureState::Done(m::result(reason));
    }
}

impl Drop for MeasureSourceDownloadGuard<'_> {
    fn drop(&mut self) {
        let status = match self.state {
            MeasureState::Pending => "canceled",
            MeasureState::Done(status) => status,
        };

        let duration = self.creation_time.elapsed();
        let metric_name = format!("{}.duration", self.task_name);
        metric!(
            timer(&metric_name) = duration,
            "status" => status,
            "source" => self.source_name,
        );

        if let Some(bytes_transferred) = self.bytes_transferred {
            // Times are recorded in milliseconds, so match that unit when calculating throughput,
            // recording a byte / ms value.
            // This falls back to the throughput being equivalent to the amount of bytes transferred
            // if the duration is zero, or there are any conversion errors.
            let throughput = (bytes_transferred as u128)
                .checked_div(duration.as_millis())
                .and_then(|t| t.try_into().ok())
                .unwrap_or(bytes_transferred);
            let throughput_name = format!("{}.throughput", self.task_name);
            metric!(
                histogram(&throughput_name) = throughput,
                "status" => status,
                "source" => self.source_name,
            );
        }
    }
}

/// Measures the timing of a download-related future and reports metrics as a histogram.
///
/// This function reports a single metric corresponding to the task name. This metric is reported
/// regardless of the future's return value.
///
/// A tag with the source name is also added to the metric, in addition to a tag recording the
/// status of the future.
pub fn measure_download_time<'a, F, T, E>(
    source_name: &'a str,
    f: F,
) -> impl Future<Output = F::Output> + 'a
where
    F: 'a + Future<Output = Result<T, E>>,
{
    let guard = MeasureSourceDownloadGuard::new("source.download.connect", source_name);
    async move {
        let output = f.await;
        guard.done(&output);
        output
    }
}

/// Computes a download timeout based on a content length in bytes and a per-gigabyte timeout.
///
/// Returns `content_length / 2^30 * timeout_per_gb`, with a minimum value of 10s.
fn content_length_timeout(content_length: u32, timeout_per_gb: Duration) -> Duration {
    let gb = content_length as f64 / (1024.0 * 1024.0 * 1024.0);
    timeout_per_gb.mul_f64(gb).max(Duration::from_secs(10))
}

#[cfg(test)]
mod tests {
    // Actual implementation is tested in the sub-modules, this only needs to
    // ensure the service interface works correctly.

    use symbolicator_sources::{HttpRemoteFile, ObjectType, SourceConfig};

    use super::*;

    use crate::test;

    #[tokio::test]
    async fn test_download() {
        test::setup();

        let (_srv, source) = test::symbol_server();
        let file_source = match source {
            SourceConfig::Http(source) => {
                HttpRemoteFile::new(source, SourceLocation::new("hello.txt")).into()
            }
            _ => panic!("unexpected source"),
        };

        let config = Config {
            connect_to_reserved_ips: true,
            ..Config::default()
        };

        let service = DownloadService::new(&config, tokio::runtime::Handle::current());

        // Jump through some hoops here, to prove that we can .await the service.
        match service
            .download(file_source, tempfile::NamedTempFile::new().unwrap())
            .await
        {
            Ok(DownloadStatus::Completed(temp_file)) => {
                let content = std::fs::read_to_string(temp_file.path()).unwrap();
                assert_eq!(content, "hello world\n")
            }
            _ => panic!("download should be completed"),
        }
    }

    #[tokio::test]
    async fn test_list_files() {
        test::setup();

        let source = test::local_source();
        let objid = ObjectId {
            code_id: Some("5ab380779000".parse().unwrap()),
            code_file: Some("C:\\projects\\breakpad-tools\\windows\\Release\\crash.exe".into()),
            debug_id: Some("3249d99d-0c40-4931-8610-f4e4fb0b6936-1".parse().unwrap()),
            debug_file: Some("C:\\projects\\breakpad-tools\\windows\\Release\\crash.pdb".into()),
            object_type: ObjectType::Pe,
        };

        let config = Config::default();
        let svc = DownloadService::new(&config, tokio::runtime::Handle::current());
        let file_list = svc
            .list_files(&[source.clone()], FileType::all(), &objid)
            .await;

        assert!(!file_list.is_empty());
        let item = &file_list[0];
        assert_eq!(item.source_id(), source.id());
    }

    #[test]
    fn test_content_length_timeout() {
        let timeout_per_gb = Duration::from_secs(30);
        let one_gb = 1024 * 1024 * 1024;

        let timeout = |content_length| content_length_timeout(content_length, timeout_per_gb);

        // very short file
        assert_eq!(timeout(100), Duration::from_secs(10));

        // 0.5 GB
        assert_eq!(timeout(one_gb / 2), timeout_per_gb / 2);

        // 1 GB
        assert_eq!(timeout(one_gb), timeout_per_gb);

        // 1.5 GB
        assert_eq!(timeout(one_gb * 3 / 2), timeout_per_gb.mul_f64(1.5));
    }
}
