//! Support to download from S3 buckets.
//!
//! Specifically this supports the [`S3SourceConfig`] source.

use std::any::type_name;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::BytesMut;
use futures::compat::{Future01CompatExt, Stream01CompatExt};
use futures01::Stream;
use parking_lot::Mutex;
use rusoto_s3::S3;
use tokio::codec::{BytesCodec, FramedRead};

use super::{DownloadError, DownloadStatus};
use crate::sources::{FileType, ObjectFileSource, S3ObjectFileSource, S3SourceConfig, S3SourceKey};
use crate::types::ObjectId;

type ClientCache = lru::LruCache<Arc<S3SourceKey>, Arc<rusoto_s3::S3Client>>;

/// Maximum number of cached S3 clients.
///
/// This number defines the size of the internal cache for S3 clients and should be higher than
/// expected concurrency across S3 buckets. If this number is too low, the downloader will
/// re-authenticate between every request.
///
/// TODO(ja):
/// This can be monitored with the `source.gcs.token.requests` and `source.gcs.token.cached` counter
/// metrics.
const S3_CLIENT_CACHE_SIZE: usize = 100;

/// Downloader implementation that supports the [`S3SourceConfig`] source.
pub struct S3Downloader {
    http_client: Arc<rusoto_core::HttpClient>,
    client_cache: Mutex<ClientCache>,
}

impl fmt::Debug for S3Downloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(type_name::<Self>())
            .field("http_client", &format_args!("rusoto_core::HttpClient"))
            .field("client_cache", &self.client_cache)
            .finish()
    }
}

impl S3Downloader {
    pub fn new() -> Self {
        Self {
            http_client: Arc::new(rusoto_core::HttpClient::new().unwrap()),
            client_cache: Mutex::new(ClientCache::new(S3_CLIENT_CACHE_SIZE)),
        }
    }

    fn get_s3_client(&self, key: &Arc<S3SourceKey>) -> Arc<rusoto_s3::S3Client> {
        let mut container = self.client_cache.lock();
        if let Some(client) = container.get(&*key) {
            metric!(counter("source.s3.client.cached") += 1);
            client.clone()
        } else {
            metric!(counter("source.s3.client.create") += 1);

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

    pub async fn download_source(
        &self,
        file_source: S3ObjectFileSource,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let key = file_source.key();
        let bucket = file_source.bucket();
        log::debug!("Fetching from s3: {} (from {})", &key, &bucket);

        let source_key = &file_source.source.source_key;
        let response = self
            .get_s3_client(&source_key)
            .get_object(rusoto_s3::GetObjectRequest {
                key: key.clone(),
                bucket: bucket.clone(),
                ..Default::default()
            })
            .compat()
            .await;

        match response {
            Ok(mut result) => {
                let body_read = match result.body.take() {
                    Some(body) => body.into_async_read(),
                    None => {
                        log::debug!("Empty response from s3:{}{}", bucket, &key);
                        return Ok(DownloadStatus::NotFound);
                    }
                };

                let stream = FramedRead::new(body_read, BytesCodec::new())
                    .map(BytesMut::freeze)
                    .map_err(DownloadError::Io)
                    .compat();

                super::download_stream(file_source.into(), stream, destination).await
            }
            Err(err) => {
                // For missing files, Amazon returns different status codes based on the given
                // permissions.
                // - To fetch existing objects, `GetObject` is required.
                // - If `ListBucket` is premitted, a 404 is returned for missing objects.
                // - Otherwise, a 403 ("access denied") is returned.
                log::debug!("Skipping response from s3://{}/{}: {}", bucket, &key, err);
                Ok(DownloadStatus::NotFound)
            }
        }
    }

    pub fn list_files(
        &self,
        source: Arc<S3SourceConfig>,
        filetypes: &'static [FileType],
        object_id: ObjectId,
    ) -> Vec<ObjectFileSource> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id: &object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| S3ObjectFileSource::new(source.clone(), loc).into())
        .collect()
    }
}
