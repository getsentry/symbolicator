use std::fs::File;
use std::io;
use std::sync::Arc;

use failure::Fail;
use futures::{future, Future, IntoFuture};
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::Cacher;
use crate::actors::objects::common::prepare_download_paths;
use crate::actors::objects::{
    DownloadPath, DownloadStream, FetchFileInner, FetchFileRequest, ObjectError, ObjectErrorKind,
    PrioritizedDownloads,
};
use crate::sentry::SentryFutureExt;
use crate::types::{ArcFail, FileType, FilesystemSourceConfig, ObjectId, Scope};

pub fn prepare_downloads(
    source: &Arc<FilesystemSourceConfig>,
    scope: Scope,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
    threadpool: Arc<ThreadPool>,
    cache: Arc<Cacher<FetchFileRequest>>,
) -> Box<Future<Item = PrioritizedDownloads, Error = ObjectError>> {
    let mut requests = vec![];

    for download_path in prepare_download_paths(
        object_id,
        filetypes,
        &source.files.filters,
        source.files.layout,
    ) {
        let request = cache
            .compute_memoized(FetchFileRequest {
                scope: scope.clone(),
                request: FetchFileInner::Filesystem(source.clone(), download_path),
                object_id: object_id.clone(),
                threadpool: threadpool.clone(),
            })
            .sentry_hub_new_from_current() // new hub because of join_all
            .map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into())
            .then(Ok);

        requests.push(request);
    }

    Box::new(future::join_all(requests))
}

pub(super) fn download_from_source(
    source: Arc<FilesystemSourceConfig>,
    download_path: &DownloadPath,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    let download_abspath = source.path.join(&download_path.0);
    log::debug!("Fetching debug file from {:?}", download_abspath);

    let res = match File::open(download_abspath.clone()) {
        Ok(_) => Ok(Some(DownloadStream::File(download_abspath))),
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound => Ok(None),
            _ => Err(ObjectError::from(ObjectErrorKind::Io)),
        },
    };

    Box::new(res.into_future())
}
