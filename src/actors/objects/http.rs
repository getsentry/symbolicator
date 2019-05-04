use std::sync::Arc;

use actix_web::http::header::HeaderName;
use actix_web::{client, HttpMessage};
use failure::Fail;
use futures::{future, Future, IntoFuture, Stream};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::Cacher;
use crate::actors::objects::common::prepare_download_paths;
use crate::actors::objects::{
    DownloadPath, DownloadStream, FetchFileInner, FetchFileRequest, ObjectError, ObjectErrorKind,
    PrioritizedDownloads, USER_AGENT,
};
use crate::http;
use crate::sentry::SentryFutureExt;
use crate::types::{ArcFail, FileType, HttpSourceConfig, ObjectId, Scope};

pub fn prepare_downloads(
    source: &Arc<HttpSourceConfig>,
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
                request: FetchFileInner::Http(source.clone(), download_path),
                object_id: object_id.clone(),
                threadpool: threadpool.clone(),
            })
            .sentry_hub_new_from_current()
            .map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into())
            .then(Ok);

        requests.push(request);
    }

    Box::new(future::join_all(requests))
}

pub fn download_from_source(
    source: &HttpSourceConfig,
    download_path: &DownloadPath,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    // XXX: Probably should send an error if the URL turns out to be invalid
    let download_url = match source.url.join(&download_path.0) {
        Ok(x) => x,
        Err(_) => return Box::new(Ok(None).into_future()),
    };

    log::debug!("Fetching debug file from {}", download_url);
    let response = clone!(download_url, source, || {
        http::follow_redirects(
            {
                let mut builder = client::get(&download_url);
                for (key, value) in source.headers.iter() {
                    if let Ok(key) = HeaderName::from_bytes(key.as_bytes()) {
                        builder.header(key, value.as_str());
                    }
                }
                builder.header("user-agent", USER_AGENT);
                builder.finish().unwrap()
            },
            10,
        )
    });

    let response = Retry::spawn(
        ExponentialBackoff::from_millis(100).map(jitter).take(3),
        response,
    );

    let response = response.map_err(|e| match e {
        tokio_retry::Error::OperationError(e) => e,
        e => panic!("{}", e),
    });

    let response = response.then(move |result| match result {
        Ok(response) => {
            if response.status().is_success() {
                log::trace!("Success hitting {}", download_url);
                Ok(Some(Box::new(
                    response
                        .payload()
                        .map_err(|e| e.context(ObjectErrorKind::Io).into()),
                )
                    as Box<dyn Stream<Item = _, Error = _>>))
            } else {
                log::trace!(
                    "Unexpected status code from {}: {}",
                    download_url,
                    response.status()
                );
                Ok(None)
            }
        }
        Err(e) => {
            log::warn!("Skipping response from {}: {}", download_url, e);
            Ok(None)
        }
    });

    Box::new(response)
}
