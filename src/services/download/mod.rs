//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! [https://docs.sentry.io/workflow/debug-files/#symbol-servers](https://docs.sentry.io/workflow/debug-files/#symbol-servers)

use std::io::Write;
use std::path::PathBuf;

use failure::ResultExt;
use futures::compat::Future01CompatExt;
use futures::future::{FutureExt, TryFutureExt};
use futures01::future;
use futures01::prelude::*;

use crate::utils::futures::RemoteThread;

mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;
mod types;

pub use self::types::{DownloadError, DownloadErrorKind, DownloadStatus, DownloadStream};
pub use crate::sources::{SentryFileId, SourceFileId, SourceLocation};

/// A service which can download files from a [`SourceConfig`].
///
/// The service is rather simple on the outside but will one day control
/// rate limits and the concurrency it uses.
///
/// [`SourceConfig`]: ../../types/enum.SourceConfig.html
#[derive(Debug, Clone)]
pub struct DownloadService {
    worker: RemoteThread,
}

impl DownloadService {
    pub fn new(worker: RemoteThread) -> Self {
        Self { worker }
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
    ) -> Box<dyn Future<Item = DownloadStatus, Error = DownloadError> + Send + 'static> {
        let fut03 = self.worker.spawn(|| async move {
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
        });
        let fut01 = fut03
            .map(|spawn_ret| spawn_ret.unwrap_or_else(|_| Err(DownloadErrorKind::Canceled.into())))
            .boxed()
            .compat();
        Box::new(future_metrics!(
            "service.download",
            Some((
                std::time::Duration::from_secs(3600),
                DownloadErrorKind::Canceled.into()
            )),
            fut01
        ))
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

        let svc = DownloadService::new(RemoteThread::new_threaded());
        let ret = test::block_fn(|| svc.download(source_id, dest.clone()));
        assert_eq!(ret.unwrap(), DownloadStatus::Completed);
        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n")
    }
}
