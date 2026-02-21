//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! <https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/>

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::{Duration, Instant};

use ::sentry::SentryFutureExt;
use futures::{future::Either, prelude::*};
use reqwest::StatusCode;

use crate::caching::{CacheContents, CacheError};
use crate::config::{Config, DownloadTimeouts};
use crate::download::deny_list::HostDenyList;
use crate::types::Scope;
use crate::utils::futures::{CancelOnDrop, SendFuture as _, m, measure};
use crate::utils::gcs::GcsError;
use crate::utils::sentry::ConfigureScope;
use stream::FuturesUnordered;
pub use symbolicator_sources::{
    DirectoryLayout, FileType, ObjectId, ObjectType, RemoteFile, RemoteFileUri, SourceConfig,
    SourceFilters, SourceLocation,
};
use symbolicator_sources::{
    FilesystemRemoteFile, GcsRemoteFile, HttpRemoteFile, S3RemoteFile, SourceLocationIter,
};
use tokio::io::AsyncWriteExt;

mod compression;
mod deny_list;
mod destination;
mod fetch_file;
mod filesystem;
mod gcs;
mod http;
mod index;
mod partial;
mod s3;
pub mod sentry;

pub use self::compression::tempfile_in_parent;
pub use self::destination::{Destination, MultiStreamDestination, WriteStream};
pub use self::fetch_file::fetch_file;
pub use index::SourceIndexService;

impl ConfigureScope for RemoteFile {
    fn to_scope(&self, scope: &mut ::sentry::Scope) {
        scope.set_tag("source.id", self.source_id());
        scope.set_tag("source.type", self.source_metric_key());
        scope.set_tag("source.is_public", self.is_public());
        scope.set_tag("source.uri", self.uri());
    }
}

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
    pub runtime: tokio::runtime::Handle,
    pub timeouts: DownloadTimeouts,
    pub trusted_client: reqwest::Client,
    sentry: sentry::SentryDownloader,
    http: http::HttpDownloader,
    s3: s3::S3Downloader,
    gcs: gcs::GcsDownloader,
    fs: filesystem::FilesystemDownloader,
    host_deny_list: Option<HostDenyList>,
    connect_to_reserved_ips: bool,
}

impl DownloadService {
    /// Creates a new downloader that runs all downloads in the given remote thread.
    pub fn new(config: &Config, runtime: tokio::runtime::Handle) -> Arc<Self> {
        let timeouts = config.timeouts;

        // |   client   | can connect to reserved IPs | accepts invalid SSL certs |
        // | -----------| ----------------------------|---------------------------|
        // |   trusted  |             yes             |             no            |
        // | restricted | according to config setting |             no            |
        // |   no_ssl   | according to config setting |             yes           |
        let trusted_client = crate::utils::http::create_client(&timeouts, true, false);
        let restricted_client =
            crate::utils::http::create_client(&timeouts, config.connect_to_reserved_ips, false);
        let no_ssl_client =
            crate::utils::http::create_client(&timeouts, config.connect_to_reserved_ips, true);

        let in_memory = &config.caches.in_memory;

        Arc::new(Self {
            runtime: runtime.clone(),
            timeouts,
            trusted_client: trusted_client.clone(),
            sentry: sentry::SentryDownloader::new(
                trusted_client,
                runtime,
                timeouts,
                in_memory,
                config.propagate_traces,
            ),
            http: http::HttpDownloader::new(restricted_client.clone(), no_ssl_client, timeouts),
            s3: s3::S3Downloader::new(
                restricted_client.clone(),
                timeouts,
                in_memory.s3_client_capacity,
            ),
            gcs: gcs::GcsDownloader::new(restricted_client, timeouts, in_memory.gcs_token_capacity),
            fs: filesystem::FilesystemDownloader::new(),
            host_deny_list: config
                .deny_list_enabled
                .then_some(HostDenyList::from_config(config)),
            connect_to_reserved_ips: config.connect_to_reserved_ips,
        })
    }

    /// Dispatches downloading of the given file to the appropriate source.
    async fn dispatch_download(&self, source: &RemoteFile, destination: &Path) -> CacheContents {
        let source_name = source.source_metric_key();
        let result = retry(|| async {
            // XXX: we have to create the file here, as doing so outside in `download`
            // would run into borrow checker problems due to the `&mut`.
            let mut destination = tokio::fs::File::create(destination).await?;
            let result = match source {
                RemoteFile::Sentry(source) => {
                    self.sentry
                        .download_source(source_name, source, &mut destination)
                        .await
                }
                RemoteFile::Http(source) => {
                    self.http
                        .download_source(source_name, source, &mut destination)
                        .await
                }
                RemoteFile::S3(source) => {
                    self.s3
                        .download_source(source_name, source, &mut destination)
                        .await
                }
                RemoteFile::Gcs(source) => {
                    self.gcs
                        .download_source(source_name, source, &mut destination)
                        .await
                }
                RemoteFile::Filesystem(source) => {
                    self.fs.download_source(source, &mut destination).await
                }
            };
            let _ = destination.flush().await;
            result
        })
        .await;

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
    ) -> CacheContents {
        let host = source.host();

        let is_builtin_source = source.is_builtin();
        let source_metric_key = source.source_metric_key().to_string();
        // NOTE: This allow-lists every external non-http symbol server.
        // This includes S3, GCS, and builtin http symbol servers that might misbehave.
        // If we want to tighten that up to only allow-list the sentry internal source,
        // this should be `"sentry:project"` instead, as defined here:
        // <https://github.com/getsentry/sentry/blob/b27ef04df6ecbaa0a34a472f787a163ca8400cc0/src/sentry/lang/native/sources.py#L17>
        let source_can_be_blocked = source_metric_key == "http";

        if source_can_be_blocked
            && let Some(deny_list) = self.host_deny_list.as_ref()
            && let Some(reason) = deny_list.is_blocked(&host)
        {
            metric!(counter("service.download.blocked") += 1, "source" => &source_metric_key);
            return Err(deny_list.format_error(&host, &reason));
        }

        let timeout = self.timeouts.max_download;
        let slf = self.clone();
        let job = async move { slf.dispatch_download(&source, &destination).await };
        let job = CancelOnDrop::new(self.runtime.spawn(job.bind_hub(::sentry::Hub::current())));
        let job = tokio::time::timeout(timeout, job);
        let job = measure("service.download", m::timed_result, job);

        let result = match job.await {
            // Timeout
            Err(_) => Err(CacheError::Timeout(timeout)),
            // Spawn error
            Ok(Err(_)) => Err(CacheError::InternalError),
            Ok(Ok(res)) => res,
        };

        if let Err(ref e @ (CacheError::DownloadError(_) | CacheError::Timeout(_))) = result {
            metric!(counter("service.download.failure") += 1, "source" => &source_metric_key);

            if source_metric_key == "sentry:project" {
                ::sentry::configure_scope(|scope| scope.set_tag("host", host.clone()));
                ::sentry::capture_error(e);
            }

            if let Some(ref deny_list) = self.host_deny_list
                && source_can_be_blocked
            {
                deny_list.register_failure(&source_metric_key, host, e);
            }
        }

        // Emit metrics about download result for internal sources
        if is_builtin_source {
            let status = match result {
                Ok(_) => "success",
                Err(CacheError::NotFound) => "notfound",
                Err(CacheError::PermissionDenied(_)) => "permissiondenied",
                Err(CacheError::Timeout(_)) => "timeout",
                Err(CacheError::DownloadError(_)) => "downloaderror",
                Err(CacheError::Malformed(_)) => "malformed",
                Err(CacheError::Unsupported(_)) => "unsupported",
                Err(CacheError::InternalError) => "internalerror",
            };
            metric!(counter("service.builtin_source.download") += 1, "source" => &source_metric_key, "status" => status);
        }

        result
    }

    /// Whether this download service is allowed to connect to reserved ip addresses.
    pub fn can_connect_to_reserved_ips(&self) -> bool {
        self.connect_to_reserved_ips
    }
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
    downloader: &DownloadService,
    source_index_svc: &SourceIndexService,
    sources: &[SourceConfig],
    filetypes: &[FileType],
    object_id: &ObjectId,
) -> Vec<RemoteFile> {
    let mut remote_files = vec![];

    macro_rules! check_source {
        ($source:ident => $file_ty:ty, $index:expr) => {{
            let mut iter =
                SourceLocationIter::new(&$source.files, filetypes, $index, object_id).peekable();
            if iter.peek().is_none() {
                // TODO: create a special "no file on source" `RemoteFile`?
            } else {
                remote_files.extend(iter.map(|loc| <$file_ty>::new($source.clone(), loc).into()))
            }
        }};
    }

    for source in sources {
        let index = source_index_svc.fetch_index(Scope::Global, source).await;
        match source {
            SourceConfig::Sentry(cfg) => {
                let future = downloader
                    .sentry
                    .list_files(cfg.clone(), object_id, filetypes);

                match future.await {
                    Ok(files) => remote_files.extend(files),
                    Err(error) => {
                        let error: &dyn std::error::Error = &error;
                        tracing::error!(error, "Failed to fetch file list");
                        // TODO: create a special "finding files failed" `RemoteFile`?
                    }
                }
            }
            SourceConfig::Http(cfg) => {
                let mut iter =
                    SourceLocationIter::new(&cfg.files, filetypes, index.as_ref(), object_id)
                        .peekable();
                if iter.peek().is_none() {
                    // TODO: create a special "no file on source" `RemoteFile`?
                } else {
                    remote_files.extend(iter.map(|loc| {
                        let mut file = HttpRemoteFile::new(cfg.clone(), loc);

                        // This is a special case for Portable PDB files that, when requested
                        // from the NuGet symbol server need a special `SymbolChecksum` header.
                        if let Some(checksum) = object_id.debug_checksum.as_ref() {
                            file.headers
                                .0
                                .insert("SymbolChecksum".into(), checksum.into());
                        }

                        file.into()
                    }))
                }
            }
            SourceConfig::S3(cfg) => check_source!(cfg => S3RemoteFile, index.as_ref()),
            SourceConfig::Gcs(cfg) => check_source!(cfg => GcsRemoteFile, index.as_ref()),
            SourceConfig::Filesystem(cfg) => {
                check_source!(cfg => FilesystemRemoteFile, index.as_ref())
            }
        }
    }
    remote_files
}

/// Try to run a future up to 3 times with 20 millisecond delays on failure.
pub async fn retry<G, F, T>(task_gen: G) -> CacheContents<T>
where
    G: Fn() -> F,
    F: Future<Output = CacheContents<T>>,
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

/// An error handler is used for converting download errors to [`CacheError`].
trait ErrorHandler: Sync {
    fn handle(
        &self,
        source: &str,
        response: SymResponse<'_>,
    ) -> impl Future<Output = CacheError> + Send;
}

/// Download the source with a HTTP request.
///
/// This is common functionality used by many downloaders.
async fn download_reqwest(
    source_name: &str,
    builder: reqwest::RequestBuilder,
    timeouts: &DownloadTimeouts,
    destination: impl Destination,
    error_handler: &impl ErrorHandler,
) -> CacheContents {
    let (client, request) = builder.build_split();
    let request = request?;

    // A non-get request or a request with a body is not supported for partial downloads.
    let destination = match request.method() == reqwest::Method::GET && request.body().is_none() {
        true => destination.try_into_streams().await,
        false => Err(destination),
    };

    let measure = MeasureSourceDownloadGuard::new("source.download.body", source_name);
    let request = SymRequest {
        source_name,
        request,
        client,
        timeouts,
        measure: &measure,
    };

    let result = match destination {
        Ok(destination) => {
            do_download_reqwest_range(request, destination, error_handler)
                .send()
                .await
        }
        Err(destination) => {
            let destination = std::pin::pin!(destination.into_write());
            do_download_reqwest(request, destination, error_handler)
                .send()
                .await
        }
    };

    if let Err(err) = &result {
        tracing::debug!(
            error = err as &dyn std::error::Error,
            source_name = source_name,
            "failed to download file"
        );
    }

    measure.done(&result);

    result
}

/// Downloads a remote resource into `destination` using multiple concurrent range requests.
///
/// If the server does not support range requests, the function will fallback to a normal
/// non-concurrent download of the resource.
///
/// Invariant: the passed `request` must be cloneable (contain no body).
async fn do_download_reqwest_range(
    request: SymRequest<'_>,
    mut destination: impl MultiStreamDestination,
    error_handler: &impl ErrorHandler,
) -> CacheContents {
    let source = request.to_source();

    let response = request
        .try_clone()
        // Cloning should never fail, it is an invariant of the function,
        // which was validated before.
        .ok_or(CacheError::InternalError)?
        // Supply the initial range.
        //
        // If the server does not support range requests it will give us the full file contents.
        .with_range(partial::initial_range())
        .execute()
        .await?;

    // Server supports range requests and just sent us the first batch.
    match response.content_range() {
        // Server does not know about ranges and returns us the full contents.
        None if response.status().is_success() => {
            tracing::trace!(
                "Success hitting `{source}`, but server does not support range requests"
            );
            let destination = std::pin::pin!(destination.into_write());
            response.download(destination).await
        }
        None if response.status() == StatusCode::RANGE_NOT_SATISFIABLE => {
            // This case should never happen, since our initial request is always a valid request,
            // when this happens, just retry without a range header and log the error for investigation.
            tracing::debug!(
                source_name = request.source_name,
                source = source,
                "initial partial request was rejected with 416, range not satisfiable"
            );
            drop(response);

            let destination = std::pin::pin!(destination.into_write());
            do_download_reqwest(request, destination, error_handler).await
        }
        // Server returned some generic error, we need to bubble it up.
        None => Err(error_handler.handle(&source, response).await),
        // Malformed range header.
        Some(Err(err)) => {
            // This case can happen if the server returns an invalid header or a header which does
            // not specify the total length of the resource, in which case we cancel the original
            // request and just retry without a range request.
            //
            // Either way, we do not expect this to happen.
            tracing::debug!(
                error = &err as &dyn std::error::Error,
                source_name = request.source_name,
                source = source,
                "server returned an invalid range"
            );
            drop(response);

            let destination = std::pin::pin!(destination.into_write());
            do_download_reqwest(request, destination, error_handler).await
        }
        // Server indicates it supports ranges.
        Some(Ok(content_range)) => {
            tracing::trace!(
                "Successfully requested an initial range {content_range} from `{source}`"
            );

            // This would technically not be necessary, the following code would handle this case
            // correctly. Due to a bug, existing HTTP servers may return a `Content-Length` of `1`,
            // even if the `Content-Range` indicates that there is no body.
            //
            // `Reqwest` will then attempt to validate the body against the specified
            // `Content-Length` and return an error, because the body finished too early (no bytes).
            //
            // Exit early here if the total size of the file is `0`, we know there is not supposed
            // to be any content, anyways.
            //
            // See also:
            //  - https://github.com/seanmonstar/reqwest/issues/1559
            //  - https://github.com/tower-rs/tower-http/pull/556
            if content_range.total_size == 0 {
                return Ok(());
            }

            destination.set_size(content_range.total_size).await?;

            let mut futures = FuturesUnordered::new();

            let head = response.download(destination.stream(0, content_range.range().size()));
            futures.push(Either::Left(head.send()));

            for range in partial::split(content_range) {
                let request = request
                    .try_clone()
                    .ok_or(CacheError::InternalError)?
                    .with_range(range);

                let destination = destination.stream(range.start, range.size());
                let partial = do_download_reqwest(request, destination, error_handler).send();
                futures.push(Either::Right(partial));
            }

            request.measure.set_streams(futures.len());

            while futures.try_next().await?.is_some() {}
            drop(futures);

            let _ = destination.flush().await;

            Ok(())
        }
    }
}

/// Downloads the requested resource with a single download request into the `destination`.
async fn do_download_reqwest(
    request: SymRequest<'_>,
    destination: impl WriteStream,
    error_handler: &impl ErrorHandler,
) -> CacheContents {
    let source = request.to_source();
    let response = request.execute().await?;

    let status = response.status();
    if status.is_success() {
        tracing::trace!("Success hitting `{source}`");
        response.download(destination).await
    } else {
        Err(error_handler.handle(&source, response).await)
    }
}

/// Converts a response to an error.
///
/// This error handler uses the HTTP status code to infer the [`CacheError`],
/// this works for any HTTP request, but does not consider API specific responses.
struct GenericErrorHandler;

impl ErrorHandler for GenericErrorHandler {
    async fn handle(&self, source: &str, response: SymResponse<'_>) -> CacheError {
        let status = response.status();
        debug_assert!(!status.is_success());

        if let Ok(details) = response.response.text().await {
            ::sentry::configure_scope(|scope| {
                scope.set_extra(
                    "reqwest_response_body",
                    ::sentry::protocol::Value::String(details.clone()),
                );
            });
        };

        if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
            tracing::debug!("Insufficient permissions to download `{source}`: {status}",);

            // TODO: figure out if we can log/return the whole response text
            // let details = response.text().await?;
            let details = status.to_string();

            CacheError::PermissionDenied(details)
        } else if status.is_client_error() {
            // If it's a client error, chances are it's a 404.
            tracing::debug!("Unexpected client error status code from `{source}`: {status}",);

            CacheError::NotFound
        } else if status == StatusCode::FOUND {
            tracing::debug!(
                "Potential login page detected when downloading from `{source}`: {status}",
            );

            CacheError::PermissionDenied("Potential login page detected".to_string())
        } else {
            tracing::debug!("Unexpected status code from `{source}`: {status}");

            let details = status.to_string();
            CacheError::DownloadError(details)
        }
    }
}

/// A HTTP request Symbolicator wants to make.
struct SymRequest<'a> {
    source_name: &'a str,
    request: reqwest::Request,
    client: reqwest::Client,
    timeouts: &'a DownloadTimeouts,
    measure: &'a MeasureSourceDownloadGuard<'a>,
}

impl<'a> SymRequest<'a> {
    /// Returns the source location for debugging purposes.
    fn to_source(&self) -> String {
        self.request.url().to_string()
    }

    /// Applies a [`partial::Range`] to the request.
    fn with_range(mut self, range: partial::Range) -> Self {
        let header = reqwest::header::HeaderValue::from_str(&range.to_string())
            .expect("the range header to be a always valid");

        self.request
            .headers_mut()
            .insert(reqwest::header::RANGE, header);

        self
    }

    /// Attempts to clone the request.
    ///
    /// This will fail if the request contains a body which cannot be cloned.
    /// See also: [`reqwest::Request::try_clone`].
    fn try_clone(&self) -> Option<Self> {
        Some(Self {
            source_name: self.source_name,
            request: self.request.try_clone()?,
            client: self.client.clone(),
            timeouts: self.timeouts,
            measure: self.measure,
        })
    }

    /// Executes the request and returns the corresponding [`SymResponse`].
    async fn execute(self) -> CacheContents<SymResponse<'a>> {
        let request = self.client.execute(self.request);
        let request = tokio::time::timeout(self.timeouts.head, request);
        // Use a separate measure for the head request.
        //
        // We're only interested in the total combined throughput of all concurrent requests.
        // The head requests can still be tracked individually.
        let request = measure_download_time(self.source_name, request);

        let response = request
            .await
            .map_err(|_| CacheError::Timeout(self.timeouts.head))??;

        let headers: ::sentry::protocol::value::Map<_, ::sentry::protocol::Value> = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.into())))
            .collect();

        ::sentry::configure_scope(|scope| {
            scope.set_extra(
                "reqwest_response_headers",
                ::sentry::protocol::Value::Object(headers),
            );
        });

        Ok(SymResponse {
            measure: self.measure,
            response,
        })
    }
}

/// A HTTP response Symbolicator received.
struct SymResponse<'a> {
    measure: &'a MeasureSourceDownloadGuard<'a>,
    response: reqwest::Response,
}

impl SymResponse<'_> {
    /// Returns the [`StatusCode`] of the response.
    fn status(&self) -> StatusCode {
        self.response.status()
    }

    /// Returns the content range, the server replied with.
    fn content_range(
        &self,
    ) -> Option<Result<partial::BytesContentRange, partial::InvalidBytesRange>> {
        partial::BytesContentRange::from_response(&self.response)
    }

    /// Downloads the resource into `destination`, applying timeouts.
    async fn download(self, mut destination: impl WriteStream) -> CacheContents {
        let mut stream = self.response.bytes_stream().map_err(CacheError::from);
        // Transfer the contents, into the destination and track progress.
        while let Some(chunk) = stream.next().await.transpose()? {
            self.measure.add_bytes_transferred(chunk.len() as u64);
            destination.write_buf(chunk).await?;
        }

        Ok(())
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
    bytes_transferred: AtomicU64,
    streams: AtomicUsize,
}

impl<'a> MeasureSourceDownloadGuard<'a> {
    /// Creates a new measure guard for downloading a source.
    pub fn new(task_name: &'a str, source_name: &'a str) -> Self {
        Self {
            state: MeasureState::Pending,
            task_name,
            source_name,
            bytes_transferred: AtomicU64::new(0),
            creation_time: Instant::now(),
            streams: AtomicUsize::new(0),
        }
    }

    /// Sets the amount of concurrent streams used for the download.
    ///
    /// This value will be used as a tag for the emitted metrics.
    pub fn set_streams(&self, num_streams: usize) {
        self.streams
            .store(num_streams, std::sync::atomic::Ordering::Relaxed);
    }

    /// A checked add to the amount of bytes transferred during the download.
    ///
    /// This value will be emitted when the download's future is completed or cancelled.
    pub fn add_bytes_transferred(&self, additional_bytes: u64) {
        self.bytes_transferred
            .fetch_add(additional_bytes, std::sync::atomic::Ordering::Relaxed);
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

        let streams = (*self.streams.get_mut()).to_string();

        let duration = self.creation_time.elapsed();
        metric!(
            timer("download_duration") = duration,
            "task_name" => self.task_name,
            "status" => status,
            "source" => self.source_name,
            "streams" => &streams,
        );

        let bytes_transferred = *self.bytes_transferred.get_mut();
        if bytes_transferred > 0 {
            // Times are recorded in milliseconds, so match that unit when calculating throughput,
            // recording a byte / ms value.
            // This falls back to the throughput being equivalent to the amount of bytes transferred
            // if the duration is zero, or there are any conversion errors.
            let throughput = (bytes_transferred as u128)
                .checked_div(duration.as_millis())
                .and_then(|t| t.try_into().ok())
                .unwrap_or(bytes_transferred);

            metric!(
                distribution("download_throughput") = throughput,
                "task_name" => self.task_name,
                "status" => status,
                "source" => self.source_name,
                "streams" => &streams,
            );

            metric!(
                distribution("download_size") = bytes_transferred,
                "task_name" => self.task_name,
                "status" => status,
                "source" => self.source_name,
                "streams" => &streams,
            );
        }
    }
}

/// Measures the timing of a download-related future and reports metrics as a distribution.
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

#[cfg(test)]
mod tests {
    // Actual implementation is tested in the sub-modules, this only needs to
    // ensure the service interface works correctly.

    use symbolicator_sources::{HttpSourceConfig, SourceId};
    use symbolicator_test::fixture;

    use super::*;

    use crate::{
        caching::{Caches, SharedCacheService},
        test,
    };

    fn config() -> Config {
        Config {
            connect_to_reserved_ips: true,
            ..Config::default()
        }
    }

    async fn download(location: &'static str) -> tempfile::NamedTempFile {
        let (_srv, source) = test::symbol_server();
        let file_source = match source {
            SourceConfig::Http(source) => {
                HttpRemoteFile::new(source, SourceLocation::new(location)).into()
            }
            _ => panic!("unexpected source"),
        };

        let service = DownloadService::new(&config(), tokio::runtime::Handle::current());

        let temp_file = tempfile::NamedTempFile::new().unwrap();
        service
            .download(file_source, temp_file.path().to_owned())
            .await
            .unwrap();

        temp_file
    }

    #[tokio::test]
    async fn test_download_small() {
        test::setup();

        let file = download("hello.txt").await;
        let content = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(content, "hello world\n")
    }

    /// This test requests a much larger file than [`test_download`], which will initiate a
    /// concurrent download on platforms which support it.
    #[tokio::test]
    async fn test_download_large() {
        test::setup();

        let file = download("7f/883fcdc55336d0a809b0150f09500b.debug").await;
        let content = std::fs::read(file.path()).unwrap();

        let expected =
            std::fs::read(fixture("symbols/7f/883fcdc55336d0a809b0150f09500b.debug")).unwrap();

        assert_eq!(content, expected);
    }

    /// Another download, but this time the server does not respond with a `Content-Range` header.
    ///
    /// This test is using the `garbage_data/` endpoint provided by the test framework,
    /// which is just a simple axum handler, which does not implement `Range` requests,
    /// unlike the `symbols/` endpoint.
    #[tokio::test]
    async fn test_download_missing_content_range() {
        test::setup();

        let server = test::Server::new();

        let source = Arc::new(HttpSourceConfig {
            id: SourceId::new("local"),
            url: server.url("/garbage_data/"),
            headers: Default::default(),
            files: Default::default(),
            accept_invalid_certs: false,
        });
        let file_source = HttpRemoteFile::new(source, SourceLocation::new("hello_world")).into();

        let service = DownloadService::new(&config(), tokio::runtime::Handle::current());
        let file = tempfile::NamedTempFile::new().unwrap();
        service
            .download(file_source, file.path().to_owned())
            .await
            .unwrap();

        let content = std::fs::read_to_string(file.path()).unwrap();

        assert_eq!(content, "hello_world");
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
        let caches = Caches::from_config(&config).unwrap();
        let shared_cache = SharedCacheService::new(None, tokio::runtime::Handle::current());
        let svc = DownloadService::new(&config, tokio::runtime::Handle::current());
        let source_index_svc =
            SourceIndexService::new(caches.source_index, shared_cache, Arc::clone(&svc));
        let file_list = list_files(
            &svc,
            &source_index_svc,
            std::slice::from_ref(&source),
            FileType::all(),
            &objid,
        )
        .await;

        assert!(!file_list.is_empty());
        let item = &file_list[0];
        assert_eq!(item.source_id(), source.id());
    }
}
