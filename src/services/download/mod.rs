//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! [https://docs.sentry.io/workflow/debug-files/#symbol-servers](https://docs.sentry.io/workflow/debug-files/#symbol-servers)

use std::path::PathBuf;

use futures::compat::Future01CompatExt;
use futures::future::{FutureExt, TryFutureExt};
use futures01::prelude::*;

use crate::utils::futures::RemoteThread;

mod http;
mod types;

pub use self::types::{DownloadError, DownloadErrorKind, DownloadStatus};
pub use crate::sources::{SentryFileId, SourceFileId, SourceLocation};

/// A service which can download files from a [`SourceConfig`].
///
/// The service is rather simple on the outside but will one day control
/// rate limits and the concurrency it uses.
///
/// [`SourceConfig`]: ../../types/enum.SourceConfig.html
#[derive(Debug, Clone)]
pub struct Downloader {
    worker: RemoteThread,
}

impl Downloader {
    pub fn new(worker: RemoteThread) -> Self {
        Self { worker }
    }

    /// Download a file from a source and store it on the local filesystem.
    ///
    /// This does not do any deduplication of requests, every requested file is
    /// freshly downloaded.
    ///
    /// # Arguments
    ///
    /// `source` - The source to download from.
    ///
    /// `dest` - Pathname of filename to save the downloaded file into.  The
    ///    file will be created if it does not exist and truncated if it does.
    ///    On successful completion the file's contents will be the download
    ///    result.  In case of any error the file's contents is considered
    ///    garbage.
    ///
    /// # Return value
    ///
    /// On success returns `Some(dest)`, if the download failed e.g. due to an
    /// HTTP 404, `None` is returned.  If there is an error during the
    /// downloading [`DownloadError`] is returned.
    ///
    /// [`DownloadError`]: types/struct.DownloadError.html
    pub fn download(
        &self,
        source: SourceFileId,
        dest: PathBuf,
    ) -> Box<dyn Future<Item = DownloadStatus, Error = DownloadError> + Send + 'static> {
        let fut03 = self.worker.spawn(|| async move {
            match source {
                SourceFileId::Http(source, loc) => {
                    http::download_source(source, loc, dest).compat().await
                }
                _ => Err(DownloadErrorKind::Tmp.into()),
            }
        });
        let fut01 = fut03
            .map(|spawn_ret| spawn_ret.unwrap_or_else(|_| Err(DownloadErrorKind::Canceled.into())))
            .boxed()
            .compat();
        Box::new(fut01)
    }
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

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let dest = tmpfile.path().to_owned();

        let (_srv, source) = test::symbol_server();
        let source_id = match source {
            SourceConfig::Http(source) => {
                SourceFileId::Http(source, SourceLocation::new("hello.txt"))
            }
            _ => panic!("unexpected source"),
        };

        let dl_svc = Downloader::new(RemoteThread::new());
        let ret = test::block_fn(|| dl_svc.download(source_id, dest.clone()));
        assert_eq!(ret.unwrap(), DownloadStatus::Completed);
        let content = std::fs::read_to_string(dest).unwrap();
        assert_eq!(content, "hello world\n")
    }
}
