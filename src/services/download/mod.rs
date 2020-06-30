//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! [https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/](https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/)

use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use ::sentry::Hub;
use failure::{Fail, ResultExt};
use futures::compat::Future01CompatExt;
use futures::future::FutureExt;
use futures01::future;
use futures01::prelude::*;

use crate::utils::futures::RemoteThread;
use crate::utils::paths::get_directory_paths;
use crate::utils::sentry::SentryStdFutureExt;

mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

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

/// Common (transitional) type in many downloaders.
type DownloadStream =
    Box<dyn futures01::stream::Stream<Item = bytes::Bytes, Error = DownloadError>>;

/// A service which can download files from a [`SourceConfig`].
///
/// The service is rather simple on the outside but will one day control
/// rate limits and the concurrency it uses.
///
/// [`SourceConfig`]: ../../types/enum.SourceConfig.html
#[derive(Debug, Clone)]
pub struct DownloadService {
    worker: RemoteThread,
    hub: Option<Arc<Hub>>,
}

impl DownloadService {
    pub fn new(worker: RemoteThread) -> Self {
        Self { worker, hub: None }
    }

    pub fn bind_hub(&self, hub: Arc<Hub>) -> Self {
        Self {
            worker: self.worker.clone(),
            hub: Some(hub),
        }
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
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<DownloadStatus, DownloadError>>
                + Send
                + 'static,
        >,
    > {
        let hub = self.hub.clone();
        self.worker
            .spawn(
                "service.download",
                std::time::Duration::from_secs(3600),
                || {
                    let fut = async move {
                        match source {
                            SourceFileId::Sentry(source, loc) => {
                                sentry::download_source(source, loc, destination)
                                    .compat()
                                    .await
                            }
                            SourceFileId::Http(source, loc) => {
                                http::download_source(source, loc, destination)
                                    .compat()
                                    .await
                            }
                            SourceFileId::S3(source, loc) => {
                                s3::download_source(source, loc, destination).compat().await
                            }
                            SourceFileId::Gcs(source, loc) => {
                                gcs::download_source(source, loc, destination)
                                    .compat()
                                    .await
                            }
                            SourceFileId::Filesystem(source, loc) => {
                                filesystem::download_source(source, loc, destination)
                            }
                        }
                    };
                    match hub {
                        Some(hub) => fut.bind_hub(hub).left_future(),
                        None => fut.right_future(),
                    }
                },
            )
            // Map all SpawnError variants into DownloadErrorKind::Canceled.
            .map(|o| o.unwrap_or_else(|_| Err(DownloadErrorKind::Canceled.into())))
            .boxed()
    }

    pub fn list_files(
        &self,
        source: SourceConfig,
        filetypes: &'static [FileType],
        object_id: ObjectId,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<SourceFileId>, DownloadError>>
                + Send
                + 'static,
        >,
    > {
        let hub = self.hub.clone();
        self.worker
            .spawn(
                "service.download.list_files",
                std::time::Duration::from_secs(1800),
                move || {
                    let fut = async move {
                        match source {
                            SourceConfig::Sentry(cfg) => {
                                let vec = sentry::list_files(cfg, filetypes, object_id)
                                    .compat()
                                    .await?;
                                Ok(vec)
                            }
                            SourceConfig::Http(cfg) => {
                                Ok(http::list_files(cfg, filetypes, object_id))
                            }
                            SourceConfig::S3(cfg) => Ok(s3::list_files(cfg, filetypes, object_id)),
                            SourceConfig::Gcs(cfg) => {
                                Ok(gcs::list_files(cfg, filetypes, object_id))
                            }
                            SourceConfig::Filesystem(cfg) => {
                                Ok(filesystem::list_files(cfg, filetypes, object_id))
                            }
                        }
                    };
                    match hub {
                        Some(hub) => fut.bind_hub(hub).left_future(),
                        None => fut.right_future(),
                    }
                },
            )
            // Map all SpawnError variants into DownloadErrorKind::Canceled.
            .map(|o| o.unwrap_or_else(|_| Err(DownloadErrorKind::Canceled.into())))
            .boxed()
    }
}

/// Download the source from a streaming future.
///
/// These streaming futures are currently implemented per source type.
fn download_future_stream(
    source: SourceFileId,
    stream: Box<dyn Future<Item = Option<DownloadStream>, Error = DownloadError>>,
    destination: PathBuf,
) -> Box<dyn Future<Item = DownloadStatus, Error = DownloadError>> {
    // All file I/O in this function is blocking!
    let ret = stream.and_then(move |maybe_stream| match maybe_stream {
        Some(stream) => {
            log::trace!("Downloading from {}", source);
            let file = tryf!(
                std::fs::File::create(&destination).context(DownloadErrorKind::BadDestination)
            );
            let fut = stream
                .fold(file, |mut file, chunk| {
                    file.write_all(chunk.as_ref())
                        .context(DownloadErrorKind::Write)
                        .map_err(DownloadError::from)
                        .map(|_| file)
                })
                .and_then(|_| Ok(DownloadStatus::Completed));
            Box::new(fut) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
        }
        None => {
            let fut = future::ok(DownloadStatus::NotFound);
            Box::new(fut) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
        }
    });
    Box::new(ret)
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

        let svc = DownloadService::new(RemoteThread::new_threaded());
        let dest2 = dest.clone();

        // Jump through some hoops here, to prove that we can .await the service.
        let ret = test::block_fn(move || async move { svc.download(source_id, dest2).await });

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

        let svc = DownloadService::new(RemoteThread::new_threaded());
        let ret =
            test::block_fn(|| svc.list_files(source.clone(), FileType::all(), objid)).unwrap();

        assert!(ret.len() > 0);
        let item = &ret[0];
        if let SourceFileId::Filesystem(source_cfg, _loc) = item {
            assert_eq!(source_cfg.id, source.id());
        } else {
            panic!("Not a filesystem item");
        }
    }
}
