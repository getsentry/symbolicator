use std::path::PathBuf;
use std::sync::Arc;

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
use crate::utils::futures::{RemoteThread, ResultFuture, SendFuture};

fn get_object(
    client: Arc<rusoto_s3::S3Client>,
    bucket: String,
    key: String,
    temp_dir: PathBuf,
) -> ResultFuture<Option<DownloadedFile>, ObjectError> {
    let future = client
        .get_object(rusoto_s3::GetObjectRequest {
            key: key.clone(),
            bucket: bucket.clone(),
            ..Default::default()
        })
        .then(move |result| match result {
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

                Either::A(DownloadedFile::streaming(&temp_dir, stream).map(Some))
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

    Box::new(future)
}

type ClientCache = lru::LruCache<Arc<S3SourceKey>, Arc<rusoto_s3::S3Client>>;

pub(super) struct S3Downloader {
    thread: RemoteThread,
    http_client: Arc<rusoto_core::HttpClient>,
    s3_clients: Arc<Mutex<ClientCache>>,
}

impl S3Downloader {
    pub fn new(thread: RemoteThread) -> Self {
        Self {
            thread,
            http_client: Arc::new(rusoto_core::HttpClient::new().unwrap()),
            s3_clients: Arc::new(Mutex::new(ClientCache::new(100))),
        }
    }

    pub fn list_files(
        &self,
        source: Arc<S3SourceConfig>,
        filetypes: &'static [FileType],
        object_id: &ObjectId,
    ) -> SendFuture<Vec<FileId>, ObjectError> {
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

    fn get_client(&self, key: &Arc<S3SourceKey>) -> Arc<rusoto_s3::S3Client> {
        let mut container = self.s3_clients.lock();
        if let Some(client) = container.get(&*key) {
            client.clone()
        } else {
            let s3 = Arc::new(rusoto_s3::S3Client::new_with(
                self.http_client.clone(),
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

    pub fn download(
        &self,
        source: Arc<S3SourceConfig>,
        download_path: DownloadPath,
        temp_dir: PathBuf,
    ) -> SendFuture<Option<DownloadedFile>, ObjectError> {
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
        let client = self.get_client(&source_key);

        let response = self
            .thread
            .spawn(move || get_object(client, bucket, key, temp_dir))
            .map_err(|e| e.map_canceled(|| ObjectErrorKind::Canceled));

        Box::new(response)
    }
}
