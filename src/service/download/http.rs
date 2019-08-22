use std::path::PathBuf;
use std::sync::Arc;

use actix_web::{http::header, HttpMessage};
use futures::{future, future::Either, Future, Stream};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use url::Url;

use crate::service::download::common::{
    prepare_download_paths, DownloadError, DownloadErrorKind, DownloadPath, DownloadedFile,
};
use crate::service::download::{FileId, USER_AGENT};
use crate::types::{FileType, HttpSourceConfig, ObjectId};
use crate::utils::futures::{RemoteThread, ResultFuture, SendFuture};
use crate::utils::http;

const MAX_HTTP_REDIRECTS: usize = 10;

fn download(
    source: Arc<HttpSourceConfig>,
    download_url: Url,
    temp_dir: PathBuf,
) -> ResultFuture<Option<DownloadedFile>, DownloadError> {
    let try_download = clone!(download_url, source, || {
        http::follow_redirects(
            download_url.clone(),
            MAX_HTTP_REDIRECTS,
            clone!(source, |url| {
                let mut request = http::default_client().get(url);

                for (key, value) in &source.headers {
                    if let Ok(header) = header::HeaderName::from_bytes(key.as_bytes()) {
                        request = request.header(header, value.as_str());
                    }
                }

                request.header(header::USER_AGENT, USER_AGENT)
            }),
        )
    });

    let retries = ExponentialBackoff::from_millis(10).map(jitter).take(3);
    let response = Retry::spawn(retries, try_download)
        .map_err(|e| match e {
            tokio_retry::Error::OperationError(e) => e,
            tokio_retry::Error::TimerError(_) => unreachable!(),
        })
        .then(move |result| match result {
            Ok(mut response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", download_url);
                    let stream = response.take_payload().map_err(DownloadError::io);
                    Either::A(DownloadedFile::streaming(&temp_dir, stream).map(Some))
                } else {
                    log::trace!(
                        "Unexpected status code from {}: {}",
                        download_url,
                        response.status()
                    );
                    Either::B(future::ok(None))
                }
            }
            Err(e) => {
                log::trace!("Skipping response from {}: {}", download_url, e);
                Either::B(future::ok(None))
            }
        });

    Box::new(response)
}

pub(super) struct HttpDownloader {
    thread: RemoteThread,
}

impl HttpDownloader {
    pub fn new(thread: RemoteThread) -> Self {
        Self { thread }
    }

    pub fn list_files(
        &self,
        source: Arc<HttpSourceConfig>,
        filetypes: &'static [FileType],
        object_id: &ObjectId,
    ) -> SendFuture<Vec<FileId>, DownloadError> {
        let ids = prepare_download_paths(
            object_id,
            filetypes,
            &source.files.filters,
            source.files.layout,
        )
        .map(|download_path| FileId::Http(source.clone(), download_path))
        .collect();

        Box::new(future::ok(ids))
    }

    pub fn download(
        &self,
        source: Arc<HttpSourceConfig>,
        download_path: DownloadPath,
        temp_dir: PathBuf,
    ) -> SendFuture<Option<DownloadedFile>, DownloadError> {
        // XXX: Probably should send an error if the URL turns out to be invalid
        let download_url = match source.url.join(&download_path) {
            Ok(x) => x,
            Err(_) => return Box::new(future::ok(None)),
        };

        log::debug!("Fetching debug file from {}", download_url);

        let future = self
            .thread
            .spawn(move || download(source, download_url, temp_dir))
            .map_err(|e| e.map_canceled(|| DownloadErrorKind::Canceled));

        Box::new(future)
    }
}
