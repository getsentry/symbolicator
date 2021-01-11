//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! <https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/>

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ::sentry::{Hub, SentryFutureExt};
use actix_web::error::PayloadError;
use bytes::Bytes;
use failure::Fail;
use futures::prelude::*;
use thiserror::Error;

use crate::utils::futures::RemoteThread;
use crate::utils::paths::get_directory_paths;

mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

use crate::config::Config;
pub use crate::sources::{
    DirectoryLayout, FileType, ObjectFileSource, SourceConfig, SourceFilters, SourceLocation,
};
pub use crate::types::ObjectId;

/// HTTP User-Agent string to use.
const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

/// Errors happening while downloading from sources.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("failed to download")]
    Io(#[source] std::io::Error),
    #[error("failed to download stream")]
    Stream(#[source] failure::Compat<PayloadError>),
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

impl DownloadError {
    pub fn stream(err: PayloadError) -> Self {
        Self::Stream(err.compat())
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
    worker: RemoteThread,
    config: Arc<Config>,
    sentry: sentry::SentryDownloader,
    http: http::HttpDownloader,
    s3: s3::S3Downloader,
    gcs: gcs::GcsDownloader,
    fs: filesystem::FilesystemDownloader,
}

impl DownloadService {
    /// Creates a new downloader that runs all downloads in the given remote thread.
    pub fn new(worker: RemoteThread, config: Arc<Config>) -> Arc<Self> {
        Arc::new(Self {
            worker,
            config,
            sentry: sentry::SentryDownloader::new(),
            http: http::HttpDownloader::new(),
            s3: s3::S3Downloader::new(),
            gcs: gcs::GcsDownloader::new(),
            fs: filesystem::FilesystemDownloader::new(),
        })
    }

    /// Dispatches downloading of the given file to the appropriate source.
    async fn dispatch_download(
        self: Arc<Self>,
        source: ObjectFileSource,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        match source {
            ObjectFileSource::Sentry(inner) => {
                self.sentry.download_source(inner, destination).await
            }
            ObjectFileSource::Http(inner) => self.http.download_source(inner, destination).await,
            ObjectFileSource::S3(inner) => self.s3.download_source(inner, destination).await,
            ObjectFileSource::Gcs(inner) => self.gcs.download_source(inner, destination).await,
            ObjectFileSource::Filesystem(inner) => self.fs.download_source(inner, destination),
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
        source: ObjectFileSource,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let hub = Hub::current();
        let slf = self.clone();

        let spawn_result = self
            .worker
            .spawn("service.download", Duration::from_secs(300), || {
                slf.dispatch_download(source, destination).bind_hub(hub)
            })
            .await;

        // Map all SpawnError variants into DownloadError::Canceled.
        spawn_result.unwrap_or(Err(DownloadError::Canceled))
    }

    /// Returns all objects matching the [`ObjectId`] at the source.
    ///
    /// Some sources, namely all the symbol servers, simply return the locations at which a
    /// download attempt should be made without any guarantee the object is actually there.
    ///
    /// If the source needs to be contacted to get matching objects this may fail and
    /// returns a [`DownloadError`].
    pub async fn list_files(
        self: Arc<Self>,
        source: SourceConfig,
        filetypes: &'static [FileType],
        object_id: ObjectId,
        hub: Arc<Hub>,
    ) -> Result<Vec<ObjectFileSource>, DownloadError> {
        match source {
            SourceConfig::Sentry(cfg) => {
                let config = self.config.clone();
                let slf = self.clone();

                let job = move || async move {
                    slf.sentry
                        .list_files(cfg, filetypes, object_id, config)
                        .bind_hub(hub)
                        .await
                };

                let spawn_result = self
                    .worker
                    .spawn("service.download.list_files", Duration::from_secs(30), job)
                    .await;

                // Map all SpawnError variants into DownloadError::Canceled
                spawn_result.unwrap_or(Err(DownloadError::Canceled))
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
/// This is common functionality used by all many downloaders.
async fn download_stream(
    source: ObjectFileSource,
    stream: impl Stream<Item = Result<Bytes, DownloadError>>,
    destination: PathBuf,
) -> Result<DownloadStatus, DownloadError> {
    // All file I/O in this function is blocking!
    log::trace!("Downloading from {}", source);
    let mut file = File::create(&destination).map_err(DownloadError::BadDestination)?;
    futures::pin_mut!(stream);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(chunk.as_ref())
            .map_err(DownloadError::Write)?;
    }
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
    use super::*;

    use crate::sources::{HttpObjectFileSource, SourceConfig};
    use crate::test;
    use crate::types::ObjectType;

    #[test]
    fn test_download() {
        test::setup();

        // test::setup() enables logging, but this test spawns a thread where
        // logging is not captured.  For normal test runs we don't want to
        // pollute the stdout so silence logs here.  When debugging this test
        // you may want to temporarily remove this.
        log::set_max_level(log::LevelFilter::Off);

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path().to_owned();

        let (_srv, source) = test::symbol_server();
        let file_source = match source {
            SourceConfig::Http(source) => {
                HttpObjectFileSource::new(source, SourceLocation::new("hello.txt")).into()
            }
            _ => panic!("unexpected source"),
        };

        let config = Arc::new(Config::default());

        let service = DownloadService::new(RemoteThread::new_threaded(), config);
        let dest2 = dest.clone();

        // Jump through some hoops here, to prove that we can .await the service.
        let ret = test::block_fn(move || async move { service.download(file_source, dest2).await });
        assert_eq!(ret.unwrap(), DownloadStatus::Completed);
        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n")
    }

    #[test]
    fn test_list_files() {
        test::setup();

        // test::setup() enables logging, but this test spawns a thread where
        // logging is not captured.  For normal test runs we don't want to
        // pollute the stdout so silence logs here.  When debugging this test
        // you may want to temporarily remove this.
        log::set_max_level(log::LevelFilter::Off);

        let source = test::local_source();
        let objid = ObjectId {
            code_id: Some("5ab380779000".parse().unwrap()),
            code_file: Some("C:\\projects\\breakpad-tools\\windows\\Release\\crash.exe".into()),
            debug_id: Some("3249d99d-0c40-4931-8610-f4e4fb0b6936-1".parse().unwrap()),
            debug_file: Some("C:\\projects\\breakpad-tools\\windows\\Release\\crash.pdb".into()),
            object_type: ObjectType::Pe,
        };

        let config = Arc::new(Config::default());
        let svc = DownloadService::new(RemoteThread::new_threaded(), config);
        let ret = test::block_fn(|| {
            svc.list_files(source.clone(), FileType::all(), objid, Hub::current())
        })
        .unwrap();

        assert!(!ret.is_empty());
        let file_source = &ret[0];
        assert_eq!(file_source.source_id(), source.id());
    }
}
