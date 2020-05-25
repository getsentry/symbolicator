//! Service which handles all downloading from multiple kinds of sources.
//!
//! The sources are described on
//! https://docs.sentry.io/workflow/debug-files/#symbol-servers

use std::path::PathBuf;

use futures::compat::Future01CompatExt;
use futures::future::{FutureExt, TryFutureExt};
use futures01::future;
use futures01::prelude::*;

use crate::utils::futures::{RemoteCanceled, RemoteThread};

mod http;
mod types;

pub use self::types::{DownloadError, DownloadErrorKind, SentryFileId, SourceId, SourceLocation};

/// A service which can download files from a [crate::types::SourceConfig].
///
/// The service is rather simple on the outside but will one day control
/// rate limits and the concurrency it uses.
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
    /// This does not do any deduplication of requests, every
    /// requested file is freshly downloaded.
    ///
    /// # Arguments
    ///
    /// `source`: The source to download from.
    ///
    /// `dest`: Pathname of filename to save the downloaded file into.
    pub fn download(
        &self,
        source: SourceId,
        dest: PathBuf,
    ) -> Box<dyn Future<Item = PathBuf, Error = DownloadError> + Send + 'static> {
        let fut03 = self.worker.spawn(|| async move {
            match source {
                SourceId::Http(source, loc) => {
                    http::download_source(source, loc, dest).compat().await
                }
                _ => Err(DownloadErrorKind::Tmp.into()),
            }
        });
        let fut01 = fut03
            .then(|spawn_ret| {
                let result = match spawn_ret {
                    Ok(dl_ret) => dl_ret,
                    Err(RemoteCanceled) => Err(DownloadErrorKind::Tmp.into()),
                };
                futures::future::ready(result)
            })
            .compat();
        Box::new(fut01)
    }
}
