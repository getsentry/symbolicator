//! Support to download from S3 buckets.
//!
//! Specifically this supports the [`S3SourceConfig`] source, which allows
//! to fetch files which were directly uploaded to Sentry itself.
//!
//! [`S3SourceConfig`]: ../../../sources/struct.S3SourceConfig.html

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use failure::ResultExt;
use futures01::future;
use futures01::prelude::*;
use parking_lot::Mutex;
use rusoto_s3::S3;
use tokio::codec::{BytesCodec, FramedRead};

use super::types::{DownloadError, DownloadErrorKind, DownloadStatus, DownloadStream};
use crate::sources::{S3SourceConfig, S3SourceKey, SourceLocation};

lazy_static::lazy_static! {
    static ref AWS_HTTP_CLIENT: rusoto_core::HttpClient = rusoto_core::HttpClient::new().unwrap();
    static ref S3_CLIENTS: Mutex<lru::LruCache<Arc<S3SourceKey>, Arc<rusoto_s3::S3Client>>> =
        Mutex::new(lru::LruCache::new(100));
}

struct SharedHttpClient;

impl rusoto_core::DispatchSignedRequest for SharedHttpClient {
    type Future = rusoto_core::request::HttpClientFuture;

    fn dispatch(
        &self,
        request: rusoto_core::signature::SignedRequest,
        timeout: Option<Duration>,
    ) -> Self::Future {
        AWS_HTTP_CLIENT.dispatch(request, timeout)
    }
}

/// Download from an S3 source.
///
/// See [`Downloader::download`] for the semantics of the file being written at `dest`.
///
/// ['Downloader::download`]: ../func.download.html
pub fn download_source(
    source: Arc<S3SourceConfig>,
    location: SourceLocation,
    dest: PathBuf,
) -> Box<dyn Future<Item = DownloadStatus, Error = DownloadError>> {
    // All file I/O in this function is blocking!
    let source2 = source.clone();
    let ret =
        download_from_source(source, &location).and_then(move |maybe_stream| match maybe_stream {
            Some(stream) => {
                log::trace!(
                    "Downloading file for Sentry source {id} location {loc}",
                    id = source2.id,
                    loc = location
                );
                let file =
                    tryf!(std::fs::File::create(&dest).context(DownloadErrorKind::BadDestination));
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
    Box::new(ret) as Box<dyn Future<Item = DownloadStatus, Error = DownloadError>>
}

fn get_s3_client(key: &Arc<S3SourceKey>) -> Arc<rusoto_s3::S3Client> {
    let mut container = S3_CLIENTS.lock();
    if let Some(client) = container.get(&*key) {
        client.clone()
    } else {
        let s3 = Arc::new(rusoto_s3::S3Client::new_with(
            SharedHttpClient,
            rusoto_credential::StaticProvider::new_minimal(
                key.access_key.clone(),
                key.secret_key.clone(),
            ),
            key.region.clone(),
        ));
        container.put(key.clone(), s3.clone());
        s3
    }
}

fn download_from_source(
    source: Arc<S3SourceConfig>,
    download_path: &SourceLocation,
) -> Box<dyn Future<Item = Option<DownloadStream>, Error = DownloadError>> {
    let key = {
        let prefix = source.prefix.trim_matches(&['/'][..]);
        if prefix.is_empty() {
            download_path.0.clone()
        } else {
            format!("{}/{}", prefix, download_path.0)
        }
    };

    log::debug!("Fetching from s3: {} (from {})", &key, source.bucket);

    let bucket = source.bucket.clone();
    let source_key = &source.source_key;
    let response = get_s3_client(&source_key).get_object(rusoto_s3::GetObjectRequest {
        key: key.clone(),
        bucket: bucket.clone(),
        ..Default::default()
    });

    let response = response.then(move |result| match result {
        Ok(mut result) => {
            let body_read = match result.body.take() {
                Some(body) => body.into_async_read(),
                None => {
                    log::debug!("Empty response from s3:{}{}", bucket, &key);
                    return Ok(None);
                }
            };

            let bytes = FramedRead::new(body_read, BytesCodec::new())
                .map(BytesMut::freeze)
                .map_err(|_err| DownloadError::from(DownloadErrorKind::Io));

            Ok(Some(Box::new(bytes) as Box<dyn Stream<Item = _, Error = _>>))
        }
        Err(err) => {
            // For missing files, Amazon returns different status codes based on the given
            // permissions.
            // - To fetch existing objects, `GetObject` is required.
            // - If `ListBucket` is premitted, a 404 is returned for missing objects.
            // - Otherwise, a 403 ("access denied") is returned.
            log::debug!("Skipping response from s3:{}{}: {}", bucket, &key, err);
            Ok(None)
        }
    });

    Box::new(response)
}
