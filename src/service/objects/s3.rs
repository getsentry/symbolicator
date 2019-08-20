use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use futures::{future, future::Either, Future, Stream};
use parking_lot::Mutex;
use rusoto_s3::S3;
use tokio::codec::{BytesCodec, FramedRead};

use crate::service::objects::common::{
    prepare_download_paths, DownloadPath, DownloadedFile, ObjectError, ObjectErrorKind,
};
use crate::service::objects::FileId;
use crate::types::{FileType, ObjectId, S3SourceConfig, S3SourceKey};
use crate::utils::futures::ResultFuture;

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

pub(super) fn prepare_downloads(
    source: &Arc<S3SourceConfig>,
    filetypes: &'static [FileType],
    object_id: &ObjectId,
) -> ResultFuture<Vec<FileId>, ObjectError> {
    let ids = prepare_download_paths(
        object_id,
        filetypes,
        &source.files.filters,
        source.files.layout,
    )
    .map(|download_path| FileId::S3(source.clone(), download_path))
    .collect();
    Box::new(future::ok(ids))
}

pub(super) fn download_from_source(
    download_dir: PathBuf,
    source: Arc<S3SourceConfig>,
    download_path: &DownloadPath,
) -> ResultFuture<Option<DownloadedFile>, ObjectError> {
    let key = {
        let prefix = source.prefix.trim_matches(&['/'][..]);
        if prefix.is_empty() {
            download_path.to_string()
        } else {
            format!("{}/{}", prefix, download_path)
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
                    return Either::B(future::ok(None));
                }
            };

            let stream = FramedRead::new(body_read, BytesCodec::new())
                .map(BytesMut::freeze)
                .map_err(|_err| ObjectError::from(ObjectErrorKind::Io));

            Either::A(DownloadedFile::streaming(&download_dir, stream).map(Some))
        }
        Err(err) => {
            // For missing files, Amazon returns different status codes based on the given
            // permissions.
            // - To fetch existing objects, `GetObject` is required.
            // - If `ListBucket` is premitted, a 404 is returned for missing objects.
            // - Otherwise, a 403 ("access denied") is returned.
            log::debug!("Skipping response from s3:{}{}: {}", bucket, &key, err);
            Either::B(future::ok(None))
        }
    });

    Box::new(response)
}
