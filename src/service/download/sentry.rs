use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::{http::header, web::Bytes, HttpMessage};
use futures::{future, future::Either, Future, Stream};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::Url;

use crate::service::download::common::{
    DownloadError, DownloadErrorKind, DownloadPath, DownloadedFile, ObjectDownloader, USER_AGENT,
};
use crate::types::{FileType, ObjectId, SentrySourceConfig};
use crate::utils::futures::{FutureExt, RemoteThread, ResultFuture, SendFuture};
use crate::utils::http;

/// The minimum required fields to parse a response.
#[derive(Clone, Debug, Deserialize)]
struct SearchResult {
    id: DownloadPath,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SearchQuery {
    index_url: Url,
    token: String,
}

fn search(index_url: Url, token: String) -> ResultFuture<Bytes, DownloadError> {
    let index_request = move || {
        http::unsafe_client()
            .get(index_url.as_str())
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::AUTHORIZATION, format!("Bearer {}", token.clone()))
            .send()
            .map_err(DownloadError::io)
            .and_then(move |mut response| {
                if response.status().is_success() {
                    log::trace!("Success fetching index from Sentry");
                    Either::A(response.body().map_err(DownloadError::io))
                } else {
                    let message = format!("Sentry returned status code {}", response.status());
                    log::warn!("{}", message);
                    Either::B(future::err(DownloadError::io(message)))
                }
            })
    };

    let retries = ExponentialBackoff::from_millis(10).map(jitter).take(3);
    let index_request = Retry::spawn(retries, index_request).map_err(|e| match e {
        tokio_retry::Error::OperationError(e) => e,
        tokio_retry::Error::TimerError(_) => unreachable!(),
    });

    Box::new(index_request)
}

fn download(
    download_url: Arc<Url>,
    token: String,
    temp_dir: PathBuf,
) -> ResultFuture<Option<DownloadedFile>, DownloadError> {
    let try_download = clone!(download_url, || {
        http::unsafe_client()
            .get(download_url.as_str())
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .send()
    });

    let response = Retry::spawn(
        ExponentialBackoff::from_millis(10).map(jitter).take(3),
        try_download,
    );

    let response = response.map_err(|e| match e {
        tokio_retry::Error::OperationError(e) => e,
        tokio_retry::Error::TimerError(_) => unreachable!(),
    });

    let response = response.then(move |result| match result {
        Ok(mut response) => {
            if response.status().is_success() {
                log::trace!("Success hitting {}", download_url);
                let stream = response.take_payload().map_err(DownloadError::io);
                Either::A(DownloadedFile::streaming(&temp_dir, stream).map(Some))
            } else {
                log::debug!(
                    "Unexpected status code from {}: {}",
                    download_url,
                    response.status()
                );
                Either::B(future::ok(None))
            }
        }
        Err(e) => {
            log::warn!("Skipping response from {}: {}", download_url, e);
            Either::B(future::ok(None))
        }
    });

    Box::new(response)
}

type SearchCache = lru::LruCache<SearchQuery, (Instant, Arc<Vec<SearchResult>>)>;

pub struct SentryDownloader {
    thread: RemoteThread,
    cache: Arc<Mutex<SearchCache>>,
}

impl SentryDownloader {
    pub fn new(thread: RemoteThread) -> Self {
        Self {
            thread,
            cache: Arc::new(Mutex::new(SearchCache::new(100_000))),
        }
    }

    fn perform_search(
        &self,
        query: SearchQuery,
    ) -> SendFuture<Arc<Vec<SearchResult>>, DownloadError> {
        if let Some((created, entries)) = self.cache.lock().get(&query) {
            if created.elapsed() < Duration::from_secs(3600) {
                return Box::new(future::ok(entries.clone()));
            }
        }

        let index_url = query.index_url.clone();
        let token = query.token.clone();
        let cache = self.cache.clone();

        log::debug!("Fetching list of Sentry debug files from {}", index_url);

        let future = self
            .thread
            .spawn(move || search(index_url, token))
            .map_err(|e| e.map_canceled(|| DownloadErrorKind::Canceled))
            .and_then(|data| {
                serde_json::from_slice::<Vec<SearchResult>>(&data)
                    .map(Arc::new)
                    .map_err(DownloadError::io)
            })
            .inspect(move |entries| {
                cache.lock().put(query, (Instant::now(), entries.clone()));
            })
            .measure("downloads.sentry.index");

        Box::new(future)
    }
}

impl ObjectDownloader for SentryDownloader {
    type Config = Arc<SentrySourceConfig>;
    type ListResponse = SendFuture<Vec<DownloadPath>, DownloadError>;
    type DownloadResponse = SendFuture<Option<DownloadedFile>, DownloadError>;

    fn list_files(
        &self,
        source: Self::Config,
        _filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> Self::ListResponse {
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

        let future = self
            .perform_search(SearchQuery {
                index_url,
                token: source.token.clone(),
            })
            .map(|results| results.iter().map(|res| res.id.clone()).collect());

        Box::new(future)
    }

    fn download(
        &self,
        source: Self::Config,
        download_path: DownloadPath,
        temp_dir: PathBuf,
    ) -> Self::DownloadResponse {
        let mut url = source.url.clone();
        url.query_pairs_mut().append_pair("id", &download_path);
        let url = Arc::new(url);

        log::debug!("Fetching debug file from {}", url);
        let token = source.token.clone();

        let future = self
            .thread
            .spawn(move || download(url, token, temp_dir))
            .map_err(|e| e.map_canceled(|| DownloadErrorKind::Canceled));

        Box::new(future)
    }
}
