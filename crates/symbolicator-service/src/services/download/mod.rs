//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! <https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/>

use std::convert::TryInto;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::sentry::SentryFutureExt;
use futures::prelude::*;
use reqwest::StatusCode;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

pub use symbolicator_sources::{
    DirectoryLayout, FileType, ObjectId, ObjectType, RemoteFile, RemoteFileUri, SourceConfig,
    SourceFilters, SourceLocation,
};
use symbolicator_sources::{
    FilesystemRemoteFile, GcsRemoteFile, HttpRemoteFile, S3RemoteFile, SourceLocationIter,
};

use crate::caching::{CacheEntry, CacheError};
use crate::config::{CacheConfigs, Config, InMemoryCacheConfig};
use crate::utils::futures::{m, measure, CancelOnDrop};
use crate::utils::gcs::GcsError;
use crate::utils::sentry::ConfigureScope;

mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

impl ConfigureScope for RemoteFile {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        scope.set_tag("source.id", self.source_id());
        scope.set_tag("source.type", self.source_metric_key());
        scope.set_tag("source.is_public", self.is_public());
        scope.set_tag("source.uri", self.uri());
    }
}

/// HTTP User-Agent string to use.
const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

impl CacheError {
    fn download_error(mut error: &dyn Error) -> Self {
        while let Some(src) = error.source() {
            error = src;
        }

        let mut error_string = error.to_string();

        // Special-case a few error strings
        if error_string.contains("certificate verify failed") {
            error_string = "certificate verify failed".to_string();
        }

        if error_string.contains("SSL routines") {
            error_string = "SSL error".to_string();
        }

        Self::DownloadError(error_string)
    }
}

impl From<reqwest::Error> for CacheError {
    fn from(error: reqwest::Error) -> Self {
        Self::download_error(&error)
    }
}

impl From<GcsError> for CacheError {
    fn from(error: GcsError) -> Self {
        Self::DownloadError(error.to_string())
    }
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
    async fn dispatch_download(&self, source: &RemoteFile, destination: &Path) -> CacheEntry {
        let result = retry(|| async {
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

        let result = result.await;

        if let Err(err) = &result {
            tracing::debug!("File `{}` fetching failed: {}", source, err);
        } else {
            tracing::debug!("File `{}` fetched successfully", source);
        }

        result
    }

    /// Download a file from a source and store it on the local filesystem.
    ///
    /// This does not do any deduplication of requests, every requested file is freshly downloaded.
    ///
    /// The downloaded file is saved into `destination`. The file will be created if it does not
    /// exist and truncated if it does. In case of any error, the file's contents is considered
    /// garbage.
    pub async fn download(
        self: &Arc<Self>,
        source: RemoteFile,
        destination: PathBuf,
    ) -> CacheEntry {
        let slf = self.clone();
        let job = async move { slf.dispatch_download(&source, &destination).await };
        let job = CancelOnDrop::new(self.runtime.spawn(job.bind_hub(::sentry::Hub::current())));
        let job = tokio::time::timeout(self.max_download_timeout, job);
        let job = measure("service.download", m::timed_result, job);

        job.await
            .map_err(|_| CacheError::Timeout(self.max_download_timeout))? // Timeout
            .map_err(|_| CacheError::InternalError)? // Spawn error
    }

    /// Returns all objects matching the [`ObjectId`] at the source.
    ///
    /// Some sources, namely all the symbol servers, simply return the locations at which a
    /// download attempt should be made without any guarantee the object is actually there.
    ///
    /// If the source needs to be contacted to get matching objects this may fail and
    /// returns a [`CacheError`].
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
                    let timeout = Duration::from_secs(30);
                    let job = tokio::time::timeout(timeout, job);
                    let job = measure("service.download.list_files", m::timed_result, job);

                    let sentry_files = job.await.map_err(|_| CacheError::Timeout(timeout));
                    match sentry_files {
                        Ok(Ok(files)) => remote_files.extend(files),
                        Ok(Err(error)) | Err(error) => {
                            let error: &dyn std::error::Error = &error;
                            tracing::error!(error, "Failed to fetch file list");
                            // TODO: create a special "finding files failed" `RemoteFile`?
                        }
                    }
                }
                SourceConfig::Http(cfg) => {
                    let mut iter =
                        SourceLocationIter::new(&cfg.files, filetypes, object_id).peekable();
                    if iter.peek().is_none() {
                        // TODO: create a special "no file on source" `RemoteFile`?
                    } else {
                        remote_files.extend(iter.map(|loc| {
                            let mut file = HttpRemoteFile::new(cfg.clone(), loc);

                            // This is a special case for Portable PDB files that, when requested
                            // from the NuGet symbol server need a special `SymbolChecksum` header.
                            if let Some(checksum) = object_id.debug_checksum.as_ref() {
                                file.headers
                                    .insert("SymbolChecksum".into(), checksum.into());
                            }

                            file.into()
                        }))
                    }
                }
                SourceConfig::S3(cfg) => check_source!(cfg => S3RemoteFile),
                SourceConfig::Gcs(cfg) => check_source!(cfg => GcsRemoteFile),
                SourceConfig::Filesystem(cfg) => check_source!(cfg => FilesystemRemoteFile),
            }
        }
        remote_files
    }
}

/// Try to run a future up to 3 times with 20 millisecond delays on failure.
pub async fn retry<G, F, T>(mut task_gen: G) -> CacheEntry<T>
where
    G: FnMut() -> F,
    F: Future<Output = CacheEntry<T>>,
{
    let mut tries = 0;
    loop {
        tries += 1;
        let result = task_gen().await;

        // its highly unlikely we get a different result when retrying these
        let should_not_retry = matches!(
            result,
            Ok(_) | Err(CacheError::NotFound | CacheError::PermissionDenied(_))
        );

        if should_not_retry || tries >= 3 {
            break result;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Download the source from a stream.
///
/// This is common functionality used by many downloaders.
async fn download_stream(
    source: &RemoteFile,
    stream: impl Stream<Item = Result<impl AsRef<[u8]>, CacheError>>,
    destination: &Path,
    timeout: Option<Duration>,
) -> CacheEntry {
    // All file I/O in this function is blocking!
    tracing::trace!("Downloading from {}", source);
    let future = async {
        let mut file = File::create(destination).await?;
        futures::pin_mut!(stream);

        let mut throughput_recorder =
            MeasureSourceDownloadGuard::new("source.download.stream", source.source_metric_key());
        let result: CacheEntry = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                let chunk = chunk.as_ref();
                throughput_recorder.add_bytes_transferred(chunk.len() as u64);
                file.write_all(chunk).await?;
            }
            Ok(())
        }
        .await;
        throughput_recorder.done(&result);
        result?;

        file.flush().await?;
        Ok(())
    };

    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| CacheError::Timeout(timeout))?,
        None => future.await,
    }
}

async fn download_reqwest(
    source: &RemoteFile,
    builder: reqwest::RequestBuilder,
    connect_timeout: Duration,
    streaming_timeout: Duration,
    destination: &Path,
) -> CacheEntry {
    let request = builder.send();

    let request = tokio::time::timeout(connect_timeout, request);
    let request = measure_download_time(source.source_metric_key(), request);

    let timeout_err = CacheError::Timeout(connect_timeout);
    let response = request.await.map_err(|_| timeout_err)??;

    let status = response.status();
    if status.is_success() {
        tracing::trace!("Success hitting `{}`", source);

        let content_length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|hv| hv.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());

        let timeout = content_length.map(|cl| content_length_timeout(cl, streaming_timeout));
        let stream = response.bytes_stream().map_err(CacheError::from);

        download_stream(source, stream, destination, timeout).await
    } else if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
        tracing::debug!(
            "Insufficient permissions to download `{}`: {}",
            source,
            status
        );

        // TODO: figure out if we can log/return the whole response text
        // let details = response.text().await?;
        let details = status.to_string();

        Err(CacheError::PermissionDenied(details))
        // If it's a client error, chances are it's a 404.
    } else if status.is_client_error() {
        tracing::debug!(
            "Unexpected client error status code from `{}`: {}",
            source,
            status
        );

        Err(CacheError::NotFound)
    } else {
        tracing::debug!("Unexpected status code from `{}`: {}", source, status);

        let details = status.to_string();
        Err(CacheError::DownloadError(details))
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
fn content_length_timeout(content_length: i64, timeout_per_gb: Duration) -> Duration {
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
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        service
            .download(file_source, temp_file.path().to_owned())
            .await
            .unwrap();

        let content = std::fs::read_to_string(temp_file.path()).unwrap();
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
            debug_checksum: None,
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
