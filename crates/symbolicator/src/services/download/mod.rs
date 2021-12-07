//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! <https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/>

use std::convert::TryInto;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::prelude::*;
use reqwest::StatusCode;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use crate::cache::CacheStatus;
use crate::utils::futures::{self as future_utils, m, measure};
use crate::utils::paths::get_directory_paths;

mod filesystem;
mod gcs;
mod http;
mod locations;
mod s3;
mod sentry;

use crate::config::Config;
pub use crate::sources::{DirectoryLayout, FileType, SourceConfig, SourceFilters};
pub use crate::types::ObjectId;
pub use locations::{RemoteDif, RemoteDifUri, SourceLocation};

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
    Sentry(#[from] sentry::SentryError),
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

impl DownloadError {
    /// This produces a user-facing string representation of a download error if it is a variant
    /// that needs to be stored as a [`CacheStatus::CacheSpecificError`] entry in the download cache.
    pub fn for_cache(&self) -> String {
        match self {
            DownloadError::Gcs(inner) => format!("{}: {}", self, inner),
            DownloadError::Sentry(inner) => format!("{}: {}", self, inner),
            DownloadError::S3(inner) => format!("{}: {}", self, inner),
            DownloadError::Permissions => self.to_string(),
            DownloadError::CachedError(original_message) => original_message.clone(),
            _ => format!("{}", self),
        }
    }

    /// If a given cache entry is [`CacheStatus::CacheSpecificError`], this parses and extracts its
    /// contents into a [`DownloadError`]. This will return none if a
    /// non-[`CacheStatus::CacheSpecificError`] is provided.
    pub fn from_cache(status: &CacheStatus) -> Option<Self> {
        match status {
            CacheStatus::Positive => None,
            CacheStatus::Negative => None,
            CacheStatus::Malformed(_) => None,
            CacheStatus::CacheSpecificError(message) => {
                if message.starts_with(&Self::Permissions.to_string()) {
                    Some(Self::Permissions)
                } else {
                    Some(Self::CachedError(message.clone()))
                }
            }
        }
    }
}

/// Completion status of a successful download request.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum DownloadStatus {
    /// The download completed successfully and the file at the path can be used.
    Completed,
    /// The requested file was not found, there is no useful data at the provided path.
    NotFound,
}

/// A service which can download files from a [`SourceConfig`].
///
/// The service is rather simple on the outside but will one day control
/// rate limits and the concurrency it uses.
#[derive(Debug)]
pub struct DownloadService {
    config: Arc<Config>,
    sentry: sentry::SentryDownloader,
    http: http::HttpDownloader,
    s3: s3::S3Downloader,
    gcs: gcs::GcsDownloader,
    fs: filesystem::FilesystemDownloader,
}

impl DownloadService {
    /// Creates a new downloader that runs all downloads in the given remote thread.
    pub fn new(config: Arc<Config>) -> Arc<Self> {
        let trusted_client = crate::utils::http::create_client(&config, true);
        let restricted_client = crate::utils::http::create_client(&config, false);

        let Config {
            connect_timeout,
            streaming_timeout,
            ..
        } = *config;
        Arc::new(Self {
            config,
            sentry: sentry::SentryDownloader::new(
                trusted_client,
                connect_timeout,
                streaming_timeout,
            ),
            http: http::HttpDownloader::new(
                restricted_client.clone(),
                connect_timeout,
                streaming_timeout,
            ),
            s3: s3::S3Downloader::new(connect_timeout, streaming_timeout),
            gcs: gcs::GcsDownloader::new(restricted_client, connect_timeout, streaming_timeout),
            fs: filesystem::FilesystemDownloader::new(),
        })
    }

    /// Dispatches downloading of the given file to the appropriate source.
    async fn dispatch_download(
        &self,
        source: &RemoteDif,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        let result = future_utils::retry(|| async {
            match source {
                RemoteDif::Sentry(inner) => {
                    self.sentry
                        .download_source(inner.clone(), destination)
                        .await
                }
                RemoteDif::Http(inner) => {
                    self.http.download_source(inner.clone(), destination).await
                }
                RemoteDif::S3(inner) => self.s3.download_source(inner.clone(), destination).await,
                RemoteDif::Gcs(inner) => self.gcs.download_source(inner.clone(), destination).await,
                RemoteDif::Filesystem(inner) => {
                    self.fs.download_source(inner.clone(), destination).await
                }
            }
        });

        match result.await {
            Ok(status) => {
                match status {
                    DownloadStatus::Completed => {
                        log::debug!("Fetched debug file from {}", source);
                    }
                    DownloadStatus::NotFound => {
                        log::debug!("Debug file not found at {}", source);
                    }
                };
                Ok(status)
            }
            Err(err) => {
                log::debug!("Failed to fetch debug file from {}: {}", source, err);
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
        &self,
        source: RemoteDif,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        let job = self.dispatch_download(&source, destination);
        let job = tokio::time::timeout(self.config.max_download_timeout, job);
        let job = measure("service.download", m::timed_result, None, job);

        match job.await {
            Ok(result) => result,
            Err(_) => Err(DownloadError::Canceled),
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
        source: SourceConfig,
        filetypes: &[FileType],
        object_id: ObjectId,
    ) -> Result<Vec<RemoteDif>, DownloadError> {
        match source {
            SourceConfig::Sentry(cfg) => {
                let job = self
                    .sentry
                    .list_files(cfg, object_id, filetypes, &self.config);
                let job = tokio::time::timeout(Duration::from_secs(30), job);
                let job = measure("service.download.list_files", m::timed_result, None, job);

                job.await.map_err(|_| DownloadError::Canceled)?
            }
            SourceConfig::Http(cfg) => Ok(self.http.list_files(cfg, filetypes, object_id)),
            SourceConfig::S3(cfg) => Ok(self.s3.list_files(cfg, filetypes, object_id)),
            SourceConfig::Gcs(cfg) => Ok(self.gcs.list_files(cfg, filetypes, object_id)),
            SourceConfig::Filesystem(cfg) => Ok(self.fs.list_files(cfg, filetypes, object_id)),
        }
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
    source: &RemoteDif,
    stream: impl Stream<Item = Result<impl AsRef<[u8]>, DownloadError>>,
    destination: &Path,
    timeout: Option<Duration>,
) -> Result<DownloadStatus, DownloadError> {
    // All file I/O in this function is blocking!
    log::trace!("Downloading from {}", source);
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
        Ok(DownloadStatus::Completed)
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

/// Iterator to generate a list of [`SourceLocation`]s to attempt downloading.
#[derive(Debug)]
struct SourceLocationIter<'a> {
    /// Limits search to a set of filetypes.
    filetypes: std::slice::Iter<'a, FileType>,

    /// Filters from a `SourceConfig` to limit the amount of generated paths.
    filters: &'a SourceFilters,

    /// Information about the object file to be downloaded.
    object_id: &'a ObjectId,

    /// Directory from `SourceConfig` to define what kind of paths we generate.
    layout: DirectoryLayout,

    /// Remaining locations to iterate.
    next: Vec<String>,
}

impl Iterator for SourceLocationIter<'_> {
    type Item = SourceLocation;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next.is_empty() {
            if let Some(&filetype) = self.filetypes.next() {
                if !self.filters.is_allowed(self.object_id, filetype) {
                    continue;
                }
                self.next = get_directory_paths(self.layout, filetype, self.object_id);
            } else {
                return None;
            }
        }

        self.next.pop().map(SourceLocation::new)
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
    use symbolic::common::{CodeId, DebugId};
    use uuid::Uuid;

    // Actual implementation is tested in the sub-modules, this only needs to
    // ensure the service interface works correctly.
    use super::http::HttpRemoteDif;
    use super::*;

    use crate::sources::SourceConfig;
    use crate::test;
    use crate::types::ObjectType;

    #[tokio::test]
    async fn test_download() {
        test::setup();

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path();

        let (_srv, source) = test::symbol_server();
        let file_source = match source {
            SourceConfig::Http(source) => {
                HttpRemoteDif::new(source, SourceLocation::new("hello.txt")).into()
            }
            _ => panic!("unexpected source"),
        };

        let config = Arc::new(Config {
            connect_to_reserved_ips: true,
            ..Config::default()
        });

        let service = DownloadService::new(config);

        // Jump through some hoops here, to prove that we can .await the service.
        let download_status = service.download(file_source, dest).await.unwrap();
        assert_eq!(download_status, DownloadStatus::Completed);
        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n")
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

        let config = Arc::new(Config::default());
        let svc = DownloadService::new(config);
        let file_list = svc
            .list_files(source.clone(), FileType::all(), objid)
            .await
            .unwrap();

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

    #[test]
    fn test_iter_elf() {
        // Note that for ELF ObjectId *needs* to have the code_id set otherwise nothing is
        // created.
        let code_id = CodeId::new(String::from("abcdefghijklmnopqrstuvwxyz1234567890abcd"));
        let uuid = Uuid::from_slice(&code_id.as_str().as_bytes()[..16]).unwrap();
        let debug_id = DebugId::from_uuid(uuid);

        let mut all: Vec<_> = SourceLocationIter {
            filetypes: [FileType::ElfCode, FileType::ElfDebug].iter(),
            filters: &Default::default(),
            object_id: &ObjectId {
                debug_id: Some(debug_id),
                code_id: Some(code_id),
                ..Default::default()
            },
            layout: Default::default(),
            next: Default::default(),
        }
        .collect();
        all.sort();

        assert_eq!(
            all,
            [
                SourceLocation::new("ab/cdef1234567890abcd"),
                SourceLocation::new("ab/cdef1234567890abcd.debug")
            ]
        );
    }
}
