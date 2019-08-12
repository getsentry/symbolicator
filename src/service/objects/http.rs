use std::sync::Arc;
use std::time::Duration;

use actix_web::http::header;
use futures::{future, Future, IntoFuture, Stream};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;

use crate::service::objects::common::{prepare_download_paths, DownloadPath};
use crate::service::objects::{DownloadStream, FileId, ObjectError, USER_AGENT};
use crate::types::{FileType, HttpSourceConfig, ObjectId};
use crate::utils::http;

const MAX_HTTP_REDIRECTS: usize = 10;

pub(super) fn prepare_downloads(
    source: &Arc<HttpSourceConfig>,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> Box<dyn Future<Item = Vec<FileId>, Error = ObjectError>> {
    let ids = prepare_download_paths(
        object_id,
        filetypes,
        &source.files.filters,
        source.files.layout,
    )
    .map(|download_path| FileId::Http(source.clone(), download_path))
    .collect();

    Box::new(Ok(ids).into_future())
}

pub(super) fn download_from_source(
    source: Arc<HttpSourceConfig>,
    download_path: &DownloadPath,
) -> Box<dyn Future<Item = Option<DownloadStream>, Error = ObjectError>> {
    // XXX: Probably should send an error if the URL turns out to be invalid
    let download_url = match source.url.join(&download_path) {
        Ok(x) => x,
        Err(_) => return Box::new(future::ok(None)),
    };

    log::debug!("Fetching debug file from {}", download_url);

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

                request
                    .header(header::USER_AGENT, USER_AGENT)
                    // Disable timeouts. The timeout wraps the entire client response future, and
                    // thus also counts the request waiting for getting queued in the connector.
                    // Instead, rely on the outer future's timeout to cancel the request.
                    .timeout(Duration::from_secs(9999))
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
            Ok(response) => {
                if response.status().is_success() {
                    log::trace!("Success hitting {}", download_url);
                    let stream = Box::new(response.map_err(ObjectError::io));
                    Ok(Some(DownloadStream::FutureStream(stream)))
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
                log::trace!("Skipping response from {}: {}", download_url, e);
                Ok(None)
            }
        });

    Box::new(response)
}
