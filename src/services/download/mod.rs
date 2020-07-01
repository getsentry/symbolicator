//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! [https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/](https://getsentry.github.io/symbolicator/advanced/symbol-server-compatibility/)

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ::sentry::Hub;
use failure::{Fail, ResultExt};
use futures::compat::Future01CompatExt;
use futures::future::FutureExt;
use futures01::{future, Future, Stream};

use crate::utils::futures::RemoteThread;

mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;

pub use crate::sources::{SentryFileId, SourceFileId, SourceLocation};

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
}

/// Common (transitional) type in many downloaders.
type DownloadStream = Box<dyn Stream<Item = bytes::Bytes, Error = DownloadError>>;

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
    /// Creates a new downloader that runs all downloads in the given remote thread.
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
    ) -> impl std::future::Future<Output = Result<DownloadStatus, DownloadError>> {
        self.worker
            .spawn("service.download", Duration::from_secs(3600), || {
                dispatch_download(source, destination)
            })
            // Map all SpawnError variants into DownloadErrorKind::Canceled.
            .map(|o| o.unwrap_or_else(|_| Err(DownloadErrorKind::Canceled.into())))
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

#[cfg(test)]
mod tests {
    // Actual implementation is tested in the sub-modules, this only needs to
    // ensure the service interface works correctly.
    use super::*;

    use crate::sources::SourceConfig;
    use crate::test;

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

        let service = DownloadService::new(RemoteThread::new_threaded());
        let dest2 = dest.clone();

        // Jump through some hoops here, to prove that we can .await the service.
        let ret = test::block_fn(move || async move { service.download(source_id, dest2).await });
        assert_eq!(ret.unwrap(), DownloadStatus::Completed);
        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n")
    }
}
