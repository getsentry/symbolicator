use std::sync::Arc;
use std::time::Duration;

use actix_web::{client, HttpMessage};

use failure::Fail;
use futures::future::{self, Either};
use futures::{Future, IntoFuture, Stream};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::Cacher;
use crate::actors::objects::{
    DownloadStream, FetchFileRequest, FileId, ObjectError, ObjectErrorKind, PrioritizedDownloads,
    SentryFileId, USER_AGENT,
};
use crate::sentry::SentryFutureExt;
use crate::types::{ArcFail, FileType, ObjectId, Scope, SentrySourceConfig};

#[derive(Debug, Fail, Clone, Copy)]
pub enum SentryErrorKind {
    #[fail(display = "failed parsing JSON response from Sentry")]
    Parsing,

    #[fail(display = "bad status code from Sentry")]
    BadStatusCode,

    #[fail(display = "failed sending request to Sentry")]
    SendRequest,
}

#[derive(serde::Deserialize)]
struct FileEntry {
    id: String,
    // TODO: Add more fields
}

symbolic::common::derive_failure!(
    SentryError,
    SentryErrorKind,
    doc = "Errors happening while fetching data from Sentry"
);

pub fn prepare_downloads(
    source: &Arc<SentrySourceConfig>,
    scope: Scope,
    _filetypes: &'static [FileType],
    object_id: &ObjectId,
    threadpool: Arc<ThreadPool>,
    cache: Arc<Cacher<FetchFileRequest>>,
) -> Box<Future<Item = PrioritizedDownloads, Error = ObjectError>> {
    let index_url = {
        let mut url = source.url.clone();
        if let Some(ref debug_id) = object_id.debug_id {
            url.query_pairs_mut()
                .append_pair("debug_id", &debug_id.to_string());
        }

        if let Some(ref code_id) = object_id.code_id {
            url.query_pairs_mut()
                .append_pair("code_id", &code_id.to_string());
        }

        url
    };

    log::debug!("Fetching list of Sentry debug files from {}", index_url);
    let index_request = clone!(source, || {
        client::get(&index_url)
            .header("User-Agent", USER_AGENT)
            .header("Authorization", format!("Bearer {}", source.token.clone()))
            .finish()
            .unwrap()
            .send()
            .map_err(|e| e.context(SentryErrorKind::SendRequest).into())
            .and_then(move |response| {
                if response.status().is_success() {
                    log::trace!("Success fetching index from Sentry");
                    Either::A(
                        response
                            .json::<Vec<FileEntry>>()
                            .map_err(|e| e.context(SentryErrorKind::Parsing).into()),
                    )
                } else {
                    log::warn!("Sentry returned status code {}", response.status());
                    Either::B(Err(SentryError::from(SentryErrorKind::BadStatusCode)).into_future())
                }
            })
    });

    let index_request = Retry::spawn(
        ExponentialBackoff::from_millis(100).map(jitter).take(3),
        index_request,
    );

    let index_request = future_metrics!("downloads.sentry.index", None, index_request);

    Box::new(
        index_request
            .and_then(clone!(source, object_id, |entries| future::join_all(
                entries.into_iter().map(move |api_response| cache
                    .compute_memoized(FetchFileRequest {
                        scope: scope.clone(),
                        file_id: FileId::Sentry(source.clone(), SentryFileId(api_response.id),),
                        threadpool: threadpool.clone(),
                        object_id: object_id.clone(),
                    })
                    .sentry_hub_new_from_current() // new hub because of join_all
                    .map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into())
                    .then(Ok))
            )))
            .map_err(|e| match e {
                tokio_retry::Error::OperationError(e) => e.context(ObjectErrorKind::Sentry).into(),
                e => panic!("{}", e),
            }),
    )
}

pub fn download_from_source(
    source: Arc<SentrySourceConfig>,
    file_id: &SentryFileId,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    let download_url = {
        let mut url = source.url.clone();
        url.query_pairs_mut().append_pair("id", &file_id.0);
        url
    };

    log::debug!("Fetching debug file from {}", download_url);
    let token = &source.token;
    let response = clone!(token, download_url, || {
        client::get(&download_url)
            .header("User-Agent", USER_AGENT)
            .header("Authorization", format!("Bearer {}", token))
            // This timeout is for the entire HTTP download *including* the response stream
            // itself, in contrast to what the Actix-Web docs say. We have tested this
            // manually.
            //
            // The intent is to disable the timeout entirely, but there is no API for that.
            .timeout(Duration::from_secs(9999))
            .finish()
            .unwrap()
            .send()
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
                log::debug!(
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
