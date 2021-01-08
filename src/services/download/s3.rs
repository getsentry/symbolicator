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
use crate::sources::{FileType, S3SourceConfig, S3SourceKey, SourceFileId, SourceLocation};
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
        source: Arc<S3SourceConfig>,
        download_path: SourceLocation,
        destination: PathBuf,
    ) -> Result<DownloadStatus, DownloadError> {
        let key = source.get_key(&download_path);
        log::debug!("Fetching from s3: {} (from {})", &key, source.bucket);

        let bucket = source.bucket.clone();
        let source_key = &source.source_key;
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

                super::download_stream(SourceFileId::S3(source, download_path), stream, destination)
                    .await
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
    ) -> Vec<SourceFileId> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id: &object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| SourceFileId::S3(source.clone(), loc))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::sources::{CommonSourceConfig, DirectoryLayoutType, SourceId};
    use crate::test;
    use crate::types::ObjectType;

    use super::*;
    use rusoto_s3::S3Client;
    use sha1::{Digest as _, Sha1};

    /// Name of the bucket to create for testing.
    const S3_BUCKET: &str = "symbolicator-test";

    fn s3_source_key() -> Option<S3SourceKey> {
        let access_key = std::env::var("SENTRY_SYMBOLICATOR_TEST_AWS_ACCESS_KEY_ID").ok()?;
        let secret_key = std::env::var("SENTRY_SYMBOLICATOR_TEST_AWS_SECRET_ACCESS_KEY").ok()?;

        if access_key.is_empty() || secret_key.is_empty() {
            None
        } else {
            Some(S3SourceKey {
                region: rusoto_core::Region::UsEast1,
                access_key,
                secret_key,
            })
        }
    }

    macro_rules! s3_source_key {
        () => {
            match s3_source_key() {
                Some(key) => key,
                None => {
                    println!("Skipping due to missing SENTRY_SYMBOLICATOR_TEST_AWS_ACCESS_KEY_ID or SENTRY_SYMBOLICATOR_TEST_AWS_SECRET_ACCESS_KEY");
                    return;
                }
            }
        }
    }

    fn s3_source(source_key: S3SourceKey) -> Arc<S3SourceConfig> {
        Arc::new(S3SourceConfig {
            id: SourceId::new("s3-test"),
            bucket: S3_BUCKET.to_owned(),
            prefix: String::new(),
            source_key: Arc::new(source_key),
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        })
    }

    /// Creates an S3 bucket if it does not exist.
    async fn ensure_bucket(s3_client: &S3Client) {
        let head_result = s3_client
            .head_bucket(rusoto_s3::HeadBucketRequest {
                bucket: S3_BUCKET.to_owned(),
            })
            .compat()
            .await;

        match head_result {
            Ok(_) => return,
            Err(rusoto_core::RusotoError::Service(rusoto_s3::HeadBucketError::NoSuchBucket(_))) => {
                // fallthrough
            }
            Err(rusoto_core::RusotoError::Unknown(err)) if err.status == 404 => {
                // fallthrough. rusoto does not seem to detect the 404.
            }
            Err(err) => panic!("failed to check S3 bucket: {:?}", err),
        }

        s3_client
            .create_bucket(rusoto_s3::CreateBucketRequest {
                bucket: S3_BUCKET.to_owned(),
                ..Default::default()
            })
            .compat()
            .await
            .unwrap();
    }

    /// Loads a mock fixture into the S3 bucket if it does not exist.
    async fn ensure_fixture(
        s3_client: &S3Client,
        fixture: impl AsRef<Path>,
        key: impl Into<String>,
    ) {
        let key = key.into();
        let head_result = s3_client
            .head_object(rusoto_s3::HeadObjectRequest {
                bucket: S3_BUCKET.to_owned(),
                key: key.clone(),
                ..Default::default()
            })
            .compat()
            .await;

        match head_result {
            Ok(_) => return,
            Err(rusoto_core::RusotoError::Service(rusoto_s3::HeadObjectError::NoSuchKey(_))) => {
                // fallthrough
            }
            Err(err) => panic!("failed to check S3 object: {:?}", err),
        }

        s3_client
            .put_object(rusoto_s3::PutObjectRequest {
                bucket: S3_BUCKET.to_owned(),
                body: Some(std::fs::read(fixture).unwrap().into()),
                key,
                ..Default::default()
            })
            .compat()
            .await
            .unwrap();
    }

    /// Loads mock data into the S3 test bucket.
    ///
    /// This performs a series of actions:
    ///  - If the bucket does not exist, it will be created.
    ///  - Each file is checked and created if it does not exist.
    ///
    /// On error, the function panics with the error message.
    async fn setup_bucket(source_key: S3SourceKey) {
        let s3_client = S3Client::new_with(
            rusoto_core::HttpClient::new().expect("create S3 HTTP client"),
            rusoto_credential::StaticProvider::new_minimal(
                source_key.access_key,
                source_key.secret_key,
            ),
            source_key.region,
        );

        ensure_bucket(&s3_client).await;
        ensure_fixture(
            &s3_client,
            "tests/fixtures/symbols/502F/C0A5/1EC1/3E47/9998/684FA139DCA7",
            "50/2fc0a51ec13e479998684fa139dca7/debuginfo",
        )
        .await;
    }

    #[test]
    fn test_list_files() {
        test::setup();

        let source = s3_source(s3_source_key!());
        let downloader = S3Downloader::new();

        let object_id = ObjectId {
            code_id: Some("502fc0a51ec13e479998684fa139dca7".parse().unwrap()),
            code_file: Some("Foo.app/Contents/Foo".to_owned()),
            debug_id: Some("502fc0a5-1ec1-3e47-9998-684fa139dca7".parse().unwrap()),
            debug_file: Some("Foo".to_owned()),
            object_type: ObjectType::Macho,
        };

        let list = downloader.list_files(source, &[FileType::MachDebug], object_id);
        assert_eq!(list.len(), 1);

        assert_eq!(
            list[0].location(),
            SourceLocation::new("50/2fc0a51ec13e479998684fa139dca7/debuginfo")
        );
    }

    #[test]
    fn test_download_complete() {
        test::setup();

        let source_key = s3_source_key!();

        test::block_fn(|| async {
            setup_bucket(source_key.clone()).await;

            let source = s3_source(source_key);
            let downloader = S3Downloader::new();

            let tempdir = test::tempdir();
            let target_path = tempdir.path().join("myfile");

            let source_location =
                SourceLocation::new("50/2fc0a51ec13e479998684fa139dca7/debuginfo");

            let download_status = downloader
                .download_source(source, source_location, target_path.clone())
                .await
                .unwrap();

            assert_eq!(download_status, DownloadStatus::Completed);
            assert!(target_path.exists());

            let hash = Sha1::digest(&std::fs::read(target_path).unwrap());
            let hash = format!("{:x}", hash);
            assert_eq!(hash, "e0195c064783997b26d6e2e625da7417d9f63677");
        });
    }

    #[test]
    fn test_download_missing() {
        test::setup();

        let source_key = s3_source_key!();

        test::block_fn(|| async {
            setup_bucket(source_key.clone()).await;

            let source = s3_source(source_key);
            let downloader = S3Downloader::new();

            let tempdir = test::tempdir();
            let target_path = tempdir.path().join("myfile");

            let source_location = SourceLocation::new("does/not/exist");
            let download_status = downloader
                .download_source(source, source_location, target_path.clone())
                .await
                .unwrap();

            assert_eq!(download_status, DownloadStatus::NotFound);
            assert!(!target_path.exists());
        });
    }

    #[test]
    fn test_download_invalid_credentials() {
        test::setup();

        let broken_key = S3SourceKey {
            region: rusoto_core::Region::UsEast1,
            access_key: "".to_owned(),
            secret_key: "".to_owned(),
        };

        test::block_fn(|| async {
            let source = s3_source(broken_key);
            let downloader = S3Downloader::new();

            let tempdir = test::tempdir();
            let target_path = tempdir.path().join("myfile");

            let source_location = SourceLocation::new("does/not/exist");
            let download_status = downloader
                .download_source(source, source_location, target_path.clone())
                .await
                .unwrap();

            // We anticipate 403 for regularly missing files if the ListBucket permission is not
            // granted, therefore return `NotFound` instead of an authentication error.
            assert_eq!(download_status, DownloadStatus::NotFound);
            assert!(!target_path.exists());
        });
    }
}
