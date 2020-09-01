//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! [https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/](https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/)

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ::sentry::Hub;
use bytes::Bytes;
use failure::{Fail, ResultExt};
use futures::prelude::*;

use crate::utils::futures::RemoteThread;
use crate::utils::paths::get_directory_paths;
use crate::utils::sentry::SentryStdFutureExt;

mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

use crate::config::Config;
pub use crate::sources::{
    DirectoryLayout, FileType, SentryFileId, SourceConfig, SourceFileId, SourceFilters,
    SourceLocation,
};
pub use crate::types::ObjectId;

/// HTTP User-Agent string to use.
const USER_AGENT: &str = concat!("symbolicator/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Fail, Clone)]
pub enum DownloadErrorKind {
    #[fail(display = "failed to download")]
    Io,
    #[fail(display = "bad file destination")]
    BadDestination,
    #[fail(display = "failed writing the downloaded file")]
    Write,
    #[fail(display = "download was cancelled")]
    Canceled,
    #[fail(display = "failed to fetch data from Sentry")]
    Sentry,
}

symbolic::common::derive_failure!(
    DownloadError,
    DownloadErrorKind,
    doc = "Errors happening while downloading from sources."
);

/// Completion status of a successful download request.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum DownloadStatus {
    /// The download completed successfully and the file at the path can be used.
    Completed,
    /// The requested file was not found, there is no useful data at the provided path.
    NotFound,
}

/// Dispatches downloading of the given file to the appropriate source.
async fn dispatch_download(
    source: SourceFileId,
    destination: PathBuf,
) -> Result<DownloadStatus, DownloadError> {
    match source {
        SourceFileId::Sentry(source, loc) => {
            sentry::download_source(source, loc, destination).await
        }
        SourceFileId::Http(source, loc) => http::download_source(source, loc, destination).await,
        SourceFileId::S3(source, loc) => s3::download_source(source, loc, destination).await,
        SourceFileId::Gcs(source, loc) => gcs::download_source(source, loc, destination).await,
        SourceFileId::Filesystem(source, loc) => {
            filesystem::download_source(source, loc, destination)
        }
    }
}

/// A service which can download files from a [`SourceConfig`].
///
/// The service is rather simple on the outside but will one day control
/// rate limits and the concurrency it uses.
///
/// [`SourceConfig`]: ../../types/enum.SourceConfig.html
#[derive(Debug, Clone)]
pub struct DownloadService {
    worker: RemoteThread,
    config: Arc<Config>,
}

impl DownloadService {
    /// Creates a new downloader that runs all downloads in the given remote thread.
    pub fn new(worker: RemoteThread, config: Arc<Config>) -> Self {
        Self { worker, config }
    }

    /// Download a file from a source and store it on the local filesystem.
    ///
    /// This does not do any deduplication of requests, every requested file is
    /// freshly downloaded.
    ///
    /// The downloaded file is saved into `destination`.  The file will be created if it
    /// does not exist and truncated if it does.  In case of any error the file's contents
    /// is considered garbage.
    pub fn download(
        &self,
        source: SourceFileId,
        destination: PathBuf,
    ) -> impl Future<Output = Result<DownloadStatus, DownloadError>> {
        let hub = Hub::current();

        self.worker
            .spawn("service.download", Duration::from_secs(300), move || {
                dispatch_download(source, destination).bind_hub(hub)
            })
            // Map all SpawnError variants into DownloadErrorKind::Canceled.
            .map(|o| o.unwrap_or_else(|_| Err(DownloadErrorKind::Canceled.into())))
    }

    pub fn list_files(
        &self,
        source: SourceConfig,
        filetypes: &'static [FileType],
        object_id: ObjectId,
        hub: Arc<Hub>,
    ) -> impl Future<Output = Result<Vec<SourceFileId>, DownloadError>> {
        let worker = self.worker.clone();

        // The Sentry cache index should expire as soon as we attempt to retry negative caches.
        let sentry_index_cache_ttl = if self.config.cache_dir.is_some() {
            self.config.caches.downloaded.retry_misses_after
        } else {
            None
        };

        async move {
            match source {
                SourceConfig::Sentry(cfg) => {
                    worker
                        .spawn(
                            "service.download.list_files",
                            Duration::from_secs(30),
                            move || {
                                sentry::list_files(
                                    cfg,
                                    filetypes,
                                    object_id,
                                    sentry_index_cache_ttl,
                                )
                                .bind_hub(hub)
                            },
                        )
                        .await
                        // Map all SpawnError variants into DownloadErrorKind::Canceled
                        .unwrap_or_else(|_| Err(DownloadErrorKind::Canceled.into()))
                }
                SourceConfig::Http(cfg) => Ok(http::list_files(cfg, filetypes, object_id)),
                SourceConfig::S3(cfg) => Ok(s3::list_files(cfg, filetypes, object_id)),
                SourceConfig::Gcs(cfg) => Ok(gcs::list_files(cfg, filetypes, object_id)),
                SourceConfig::Filesystem(cfg) => {
                    Ok(filesystem::list_files(cfg, filetypes, object_id))
                }
            }
        }
    }
}

/// Download the source from a stream.
///
/// This is common functionality used by all many downloaders.
async fn download_stream(
    source: SourceFileId,
    stream: impl Stream<Item = Result<Bytes, DownloadError>>,
    destination: PathBuf,
) -> Result<DownloadStatus, DownloadError> {
    // All file I/O in this function is blocking!
    log::trace!("Downloading from {}", source);
    let mut file = File::create(&destination).context(DownloadErrorKind::BadDestination)?;
    futures::pin_mut!(stream);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(chunk.as_ref())
            .context(DownloadErrorKind::Write)?;
    }
    Ok(DownloadStatus::Completed)
}

/// Iterator to generate a list of filepaths to try downloading from.
///
///  - `object_id`: Information about the image we want to download.
///  - `filetypes`: Limit search to these filetypes.
///  - `filters`: Filters from a `SourceConfig` to limit the amount of generated paths.
///  - `layout`: Directory from `SourceConfig` to define what kind of paths we generate.
#[derive(Debug)]
struct SourceLocationIter<'a> {
    filetypes: std::slice::Iter<'a, FileType>,
    filters: &'a SourceFilters,
    object_id: &'a ObjectId,
    layout: DirectoryLayout,
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

    use crate::sources::SourceConfig;
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
        let source_id = match source {
            SourceConfig::Http(source) => {
                SourceFileId::Http(source, SourceLocation::new("hello.txt"))
            }
            _ => panic!("unexpected source"),
        };

        let config = Arc::new(Config::default());

        let service = DownloadService::new(RemoteThread::new_threaded(), config);
        let dest2 = dest.clone();

        // Jump through some hoops here, to prove that we can .await the service.
        let ret = test::block_fn(move || async move { service.download(source_id, dest2).await });
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
        let item = &ret[0];
        if let SourceFileId::Filesystem(source_cfg, _loc) = item {
            assert_eq!(source_cfg.id, source.id());
        } else {
            panic!("Not a filesystem item");
        }
    }
}
