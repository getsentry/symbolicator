use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use failure::Fail;
use futures::{future, Future, Stream};
use parking_lot::Mutex;
use rusoto_s3::S3;
use tokio::codec::{BytesCodec, FramedRead};
use tokio_threadpool::ThreadPool;

use crate::actors::common::cache::Cacher;
use crate::actors::objects::common::prepare_download_paths;
use crate::actors::objects::{
    DownloadPath, DownloadStream, FetchFileInner, FetchFileRequest, ObjectError, ObjectErrorKind,
    PrioritizedDownloads,
};
use crate::sentry::SentryFutureExt;
use crate::types::{ArcFail, FileType, ObjectId, S3SourceConfig, S3SourceKey, Scope};

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
    if let Some(client) = container.get(&key) {
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

pub fn prepare_downloads(
    source: &Arc<S3SourceConfig>,
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
                request: FetchFileInner::S3(source.clone(), download_path),
                object_id: object_id.clone(),
                threadpool: threadpool.clone(),
            })
            .sentry_hub_current()
            .map_err(|e| ArcFail(e).context(ObjectErrorKind::Caching).into())
            .then(Ok);

        requests.push(request);
    }

    Box::new(future::join_all(requests))
}

pub fn download_from_source(
    source: &S3SourceConfig,
    download_path: &DownloadPath,
) -> Box<Future<Item = Option<DownloadStream>, Error = ObjectError>> {
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
                .map_err(|_err| ObjectError::from(ObjectErrorKind::Io));

            Ok(Some(Box::new(bytes) as Box<dyn Stream<Item = _, Error = _>>))
        }
        Err(err) => {
            log::debug!("Skipping response from s3:{}{}: {}", bucket, &key, err);
            Ok(None)
        }
    });

    Box::new(response)
}
