//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! <https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/>

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ::sentry::{Hub, SentryFutureExt};
use futures::prelude::*;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use crate::utils::futures::{m, measure};
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
    #[error("failed to download")]
    Io(#[source] std::io::Error),
    #[error("failed to download")]
    Reqwest(#[source] reqwest::Error),
    #[error("bad file destination")]
    BadDestination(#[source] std::io::Error),
    #[error("failed writing the downloaded file")]
    Write(#[source] std::io::Error),
    #[error("download was cancelled")]
    Canceled,
    #[error("failed to fetch data from GCS")]
    Gcs(#[from] gcs::GcsError),
    #[error("failed to fetch data from Sentry")]
    Sentry(#[from] sentry::SentryError),
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
    worker: tokio::runtime::Handle,
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

        Arc::new(Self {
            config,
            worker: tokio::runtime::Handle::current(),
            sentry: sentry::SentryDownloader::new(trusted_client),
            http: http::HttpDownloader::new(restricted_client.clone()),
            s3: s3::S3Downloader::new(),
            gcs: gcs::GcsDownloader::new(restricted_client),
            fs: filesystem::FilesystemDownloader::new(),
        })
    }

    /// Dispatches downloading of the given file to the appropriate source.
    async fn dispatch_download(
        self: Arc<Self>,
        source: RemoteDif,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        match source {
            RemoteDif::Sentry(inner) => self.sentry.download_source(inner, destination).await,
            RemoteDif::Http(inner) => self.http.download_source(inner, destination).await,
            RemoteDif::S3(inner) => self.s3.download_source(inner, destination).await,
            RemoteDif::Gcs(inner) => self.gcs.download_source(inner, destination).await,
            RemoteDif::Filesystem(inner) => self.fs.download_source(inner, destination).await,
        }
    }

    /// Download a file from a source and store it on the local filesystem.
    ///
    /// This does not do any deduplication of requests, every requested file is freshly downloaded.
    ///
    /// The downloaded file is saved into `destination`. The file will be created if it does not
    /// exist and truncated if it does. In case of any error, the file's contents is considered
    /// garbage.
    //
    // NB: This takes `Arc<Self>` since it needs to spawn into the worker pool internally. Spawning
    // requires futures to be `'static`, which means there cannot be any references to an externally
    // owned downloader.
    pub async fn download(
        self: Arc<Self>,
        source: RemoteDif,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let hub = Hub::current();
        let slf = self.clone();

        // NB: Enter the tokio 1 runtime, which is required to create the timeout.
        // See: https://docs.rs/tokio/1.0.1/tokio/runtime/struct.Runtime.html#method.enter
        let _guard = self.worker.enter();
        let job = slf.dispatch_download(source, destination).bind_hub(hub);
        let job = tokio::time::timeout(Duration::from_secs(300), job);
        let job = measure("service.download", m::timed_result, job);

        // Map all SpawnError variants into DownloadError::Canceled.
        match self.worker.spawn(job).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err(DownloadError::Canceled),
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
        self: Arc<Self>,
        source: SourceConfig,
        filetypes: &[FileType],
        object_id: ObjectId,
        hub: Arc<Hub>,
    ) -> Result<Vec<RemoteDif>, DownloadError> {
        match source {
            SourceConfig::Sentry(cfg) => {
                let config = self.config.clone();
                let slf = self.clone();

                // This `async move` ensures that the `list_files` future completes before `slf`
                // goes out of scope, which ensures 'static lifetime for `spawn` below.
                let job = async move {
                    slf.sentry
                        .list_files(cfg, object_id, filetypes.clone(), config)
                        .bind_hub(hub)
                        .await
                };

                // NB: Enter the tokio 1 runtime, which is required to create the timeout.
                // See: https://docs.rs/tokio/1.0.1/tokio/runtime/struct.Runtime.html#method.enter
                let _guard = self.worker.enter();
                let job = tokio::time::timeout(Duration::from_secs(30), job);
                let job = measure("service.download.list_files", m::timed_result, job);

                // Map all SpawnError variants into DownloadError::Canceled.
                match self.worker.spawn(job).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) | Err(_) => Err(DownloadError::Canceled),
                }
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
async fn download_stream(
    source: impl Into<RemoteDif>,
    stream: impl Stream<Item = Result<impl AsRef<[u8]>, DownloadError>>,
    destination: PathBuf,
) -> Result<DownloadStatus, DownloadError> {
    // All file I/O in this function is blocking!
    log::trace!("Downloading from {}", source.into());
    let mut file = File::create(&destination)
        .await
        .map_err(DownloadError::BadDestination)?;
    futures::pin_mut!(stream);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(chunk.as_ref())
            .await
            .map_err(DownloadError::Write)?;
    }
    file.flush().await.map_err(DownloadError::Write)?;
    Ok(DownloadStatus::Completed)
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

#[cfg(test)]
mod tests {
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
        let dest = tmpfile.path().to_owned();

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
        let dest2 = dest.clone();

        // Jump through some hoops here, to prove that we can .await the service.
        let download_status = service.download(file_source, dest2).await.unwrap();
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
            .list_files(source.clone(), FileType::all(), objid, Hub::current())
            .await
            .unwrap();

        assert!(!file_list.is_empty());
        let item = &file_list[0];
        assert_eq!(item.source_id(), source.id());
    }
}
