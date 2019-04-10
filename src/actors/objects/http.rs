use std::sync::Arc;

use actix::Addr;
use actix_web::http::header::HeaderName;
use actix_web::{client, HttpMessage};
use failure::Fail;
use futures::{future, Future, IntoFuture, Stream};
use tokio_threadpool::ThreadPool;

use crate::actors::cache::{CacheActor, ComputeMemoized};
use crate::actors::objects::{
    paths::get_directory_path, DownloadPath, DownloadStream, FetchFile, FetchFileRequest,
    ObjectError, ObjectErrorKind, PrioritizedDownloads, USER_AGENT,
};
use crate::futures::measure_task;
use crate::http;
use crate::types::{ArcFail, FileType, HttpSourceConfig, ObjectId, Scope};

pub fn prepare_downloads(
    source: &Arc<HttpSourceConfig>,
    scope: Scope,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
    threadpool: Arc<ThreadPool>,
    cache: Addr<CacheActor<FetchFile>>,
) -> Box<Future<Item = PrioritizedDownloads, Error = ObjectError>> {
    let mut requests = vec![];

    for &filetype in filetypes {
        if !source.filetypes.contains(&filetype) {
            continue;
        }

        let download_path = match get_directory_path(source.layout, filetype, object_id) {
            Some(x) => DownloadPath(x),
            None => continue,
        };

        requests.push(FetchFile {
            scope: scope.clone(),
            request: FetchFileRequest::Http(source.clone(), download_path, object_id.clone()),
            threadpool: threadpool.clone(),
        });
    }

    Box::new(future::join_all(requests.into_iter().map(move |request| {
        cache
            .send(ComputeMemoized(request))
            .map_err(|e| e.context(ObjectErrorKind::Mailbox).into())
            .and_then(move |response| {
                Ok(response.map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into()))
            })
    })))
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
    let response = http::follow_redirects(
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
    .then(move |result| match result {
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

    Box::new(measure_task("downloads.http", None, response))
}
