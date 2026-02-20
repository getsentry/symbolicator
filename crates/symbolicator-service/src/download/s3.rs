//! Support to download from S3 buckets.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use aws_config::ecs::EcsCredentialsProvider;
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_sdk_s3::Client;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_smithy_types::error::metadata::ErrorMetadata;
use aws_smithy_xml::decode::{Document, XmlDecodeError, try_data};
use reqwest::StatusCode;
use symbolicator_sources::{AwsCredentialsProvider, S3Region, S3RemoteFile, S3SourceKey};

use crate::caching::{CacheContents, CacheError};
use crate::config::DownloadTimeouts;

use super::{Destination, ErrorHandler, SymResponse};

type ClientCache = moka::future::Cache<Arc<S3SourceKey>, Arc<Client>>;

/// Pre signed URLs are created with this expiry.
///
/// In practice this value does not matter for Symbolicator as the pre signed URL
/// is immediately used. Even with very slow downloads lasting longer than
/// this duration are fine.
const PRE_SIGN_EXPIRY: Duration = Duration::from_mins(15);

/// Downloader implementation that supports the S3 source.
pub struct S3Downloader {
    client_cache: ClientCache,
    http_client: reqwest::Client,
    timeouts: DownloadTimeouts,
}

impl fmt::Debug for S3Downloader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Downloader")
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl S3Downloader {
    pub fn new(
        http_client: reqwest::Client,
        timeouts: DownloadTimeouts,
        s3_client_capacity: u64,
    ) -> Self {
        Self {
            client_cache: ClientCache::new(s3_client_capacity),
            http_client,
            timeouts,
        }
    }

    async fn get_s3_client(&self, key: &Arc<S3SourceKey>) -> Arc<Client> {
        metric!(counter("source.s3.client.access") += 1);
        let init = Box::pin(async {
            metric!(counter("source.s3.client.computation") += 1);

            tracing::debug!(
                "Using AWS credentials provider: {:?}",
                key.aws_credentials_provider
            );
            Arc::new(match key.aws_credentials_provider {
                AwsCredentialsProvider::Container => {
                    self.create_s3_client(EcsCredentialsProvider::builder().build(), &key.region)
                        .await
                }
                AwsCredentialsProvider::Static => {
                    self.create_s3_client(
                        Credentials::from_keys(
                            key.access_key.clone(),
                            key.secret_key.0.as_ref(),
                            None,
                        ),
                        &key.region,
                    )
                    .await
                }
            })
        });

        self.client_cache
            .entry_by_ref(key)
            .or_insert_with(init)
            .await
            .into_value()
    }

    async fn create_s3_client(
        &self,
        provider: impl ProvideCredentials + 'static,
        region: &S3Region,
    ) -> Client {
        let mut config_loader = aws_config::from_env()
            .credentials_provider(provider)
            .region(region.region.clone());

        if let Some(endpoint_url) = &region.endpoint {
            config_loader = config_loader.endpoint_url(endpoint_url);
        };

        let config = config_loader.load().await;
        Client::new(&config)
    }

    /// Downloads a source hosted on an S3 bucket.
    #[tracing::instrument(
        skip_all,
        fields(
            source.name = &source_name,
            source.bucket = &file_source.source.bucket,
            source.prefix = &file_source.source.prefix,
        )
    )]
    pub async fn download_source(
        &self,
        source_name: &str,
        file_source: &S3RemoteFile,
        destination: impl Destination,
    ) -> CacheContents {
        let key = file_source.key();
        let bucket = file_source.bucket();
        tracing::debug!("Fetching from s3: {} (from {})", &key, &bucket);

        let source_key = file_source.source.source_key.clone();
        let client = self.get_s3_client(&source_key).await;

        let presigned = PresigningConfig::expires_in(PRE_SIGN_EXPIRY)
            .map_err(|e| CacheError::DownloadError(e.to_string()))?;

        let presigned = client
            .get_object()
            .bucket(&bucket)
            .key(&key)
            .presigned(presigned)
            .await
            .map_err(|e| CacheError::DownloadError(e.to_string()))?;

        let mut builder = self.http_client.get(presigned.uri());
        for (header_name, header_value) in presigned.headers() {
            builder = builder.header(header_name, header_value);
        }

        super::download_reqwest(
            source_name,
            builder,
            &self.timeouts,
            destination,
            &S3ErrorHandler,
        )
        .await
    }
}

struct S3ErrorHandler;

impl ErrorHandler for S3ErrorHandler {
    async fn handle(&self, source: &str, response: SymResponse<'_>) -> CacheError {
        let status = response.status();
        debug_assert!(!status.is_success());

        let body = response.response.bytes().await.ok();
        let metadata = body.and_then(|body| parse_error_metadata(&body).ok());

        if let Some(error) = metadata.as_ref().and_then(error_from_metadata) {
            tracing::debug!("Known S3 Error: {error}");
            return error;
        }

        let details = match metadata.as_ref().map(|m| (m.code(), m.message())) {
            Some((Some(code), None)) => code.to_owned(),
            Some((None, Some(message))) => format!("{status}: {message}"),
            Some((Some(code), Some(message))) => format!("{code}: {message}"),
            None | Some((None, None)) => status.to_string(),
        };

        tracing::debug!(
            "Falling back to status code error handling in {source} with status {status}: {details}"
        );

        match status {
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                CacheError::PermissionDenied(details)
            }
            StatusCode::NOT_FOUND => CacheError::NotFound,
            _ => CacheError::DownloadError(details),
        }
    }
}

fn error_from_metadata(metadata: &ErrorMetadata) -> Option<CacheError> {
    let code = metadata.code()?;

    Some(match code {
        "AuthorizationHeaderMalformed"
        | "AuthorizationQueryParametersError"
        | "AccessDenied"
        | "SignatureDoesNotMatch"
        | "InvalidAccessKeyId" => CacheError::PermissionDenied(match metadata.message() {
            Some(message) => format!("{code}: {message}"),
            None => code.to_owned(),
        }),
        "NoSuchBucket" | "NoSuchKey" | "NotFound" => CacheError::NotFound,
        _ => return None,
    })
}

/// The function parses S3 XML and returns the parsed `Code` and `Messsage` fields.
fn parse_error_metadata(body: &[u8]) -> Result<ErrorMetadata, XmlDecodeError> {
    let mut doc = Document::try_from(body)?;
    let mut root = doc.root_element()?;
    let mut builder = ErrorMetadata::builder();
    while let Some(mut tag) = root.next_tag() {
        match tag.start_el().local() {
            "Code" => {
                builder = builder.code(try_data(&mut tag)?);
            }
            "Message" => {
                builder = builder.message(try_data(&mut tag)?);
            }
            _ => {}
        }
    }
    Ok(builder.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    use aws_sdk_s3::Error as S3Error;
    use aws_sdk_s3::primitives::ByteStream;
    use sha1::{Digest as _, Sha1};
    use symbolicator_sources::{
        CommonSourceConfig, DirectoryLayoutType, RemoteFileUri, S3SecretKey, S3SourceConfig,
        SourceId, SourceLocation,
    };

    use crate::test;

    /// Name of the bucket to create for testing.
    const S3_BUCKET: &str = "symbolicator-test";

    fn s3_source_key() -> Option<S3SourceKey> {
        let access_key = std::env::var("SENTRY_SYMBOLICATOR_TEST_AWS_ACCESS_KEY_ID").ok()?;
        let secret_key = std::env::var("SENTRY_SYMBOLICATOR_TEST_AWS_SECRET_ACCESS_KEY").ok()?;

        if access_key.is_empty() || secret_key.is_empty() {
            None
        } else {
            Some(S3SourceKey {
                region: S3Region::from("us-east-1"),
                aws_credentials_provider: AwsCredentialsProvider::Static,
                access_key,
                secret_key: S3SecretKey(secret_key.into()),
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
    async fn ensure_bucket(s3_client: &Client) {
        let head_result = s3_client
            .head_bucket()
            .bucket(S3_BUCKET.to_owned())
            .send()
            .await;

        match head_result {
            Ok(_) => return,
            Err(err) => {
                let err = S3Error::from(err);
                match err {
                    S3Error::NoSuchBucket(_) | S3Error::NotFound(_) => {}
                    err => {
                        panic!("failed to check S3 bucket: {err:?}");
                    }
                }
            }
        };

        s3_client
            .create_bucket()
            .bucket(S3_BUCKET.to_owned())
            .send()
            .await
            .unwrap();
    }

    /// Loads a mock fixture into the S3 bucket if it does not exist.
    async fn ensure_fixture(s3_client: &Client, fixture: impl AsRef<Path>, key: impl Into<String>) {
        let key = key.into();
        let head_result = s3_client
            .head_object()
            .bucket(S3_BUCKET.to_owned())
            .key(key.clone())
            .send()
            .await;

        match head_result {
            Ok(_) => return,
            Err(err) => {
                let err = S3Error::from(err);
                match err {
                    S3Error::NotFound(_) => {}
                    // work around https://github.com/awslabs/aws-sdk-rust/issues/1148#issuecomment-2124123894:
                    // we seem to get a bogus `ContentLengthError` because *obviously*
                    // the `HEAD` request returns an empty body while another header reports an expected `content-length`.
                    _ if format!("{err:?}").contains("ContentLengthError") => {}
                    err => {
                        panic!("failed to check S3 object: {err:?}");
                    }
                }
            }
        };

        s3_client
            .put_object()
            .bucket(S3_BUCKET.to_owned())
            .key(key)
            .body(ByteStream::from(test::read_fixture(fixture)))
            .send()
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
        let provider = Credentials::from_keys(
            source_key.access_key.clone(),
            source_key.secret_key.0.as_ref(),
            None,
        );
        let shared_config = aws_config::from_env()
            .credentials_provider(provider)
            .region(source_key.region.region)
            .load()
            .await;
        let s3_client = Client::new(&shared_config);

        ensure_bucket(&s3_client).await;
        ensure_fixture(
            &s3_client,
            "symbols/502F/C0A5/1EC1/3E47/9998/684FA139DCA7",
            "50/2fc0a51ec13e479998684fa139dca7/debuginfo",
        )
        .await;
    }

    #[tokio::test]
    async fn test_download_complete() {
        test::setup();

        let source_key = s3_source_key!();
        setup_bucket(source_key.clone()).await;

        let source = s3_source(source_key);
        let downloader = S3Downloader::new(reqwest::Client::new(), Default::default(), 100);

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("50/2fc0a51ec13e479998684fa139dca7/debuginfo");
        let file_source = S3RemoteFile::new(source, source_location);

        let mut destination = tokio::fs::File::create(&target_path).await.unwrap();
        let download_status = downloader
            .download_source("", &file_source, &mut destination)
            .await;

        assert!(download_status.is_ok());
        assert!(target_path.exists());

        let hash = Sha1::digest(std::fs::read(target_path).unwrap());
        let hash = format!("{hash:x}");
        assert_eq!(hash, "e0195c064783997b26d6e2e625da7417d9f63677");
    }

    #[tokio::test]
    async fn test_download_missing() {
        test::setup();

        let source_key = s3_source_key!();
        setup_bucket(source_key.clone()).await;

        let source = s3_source(source_key);
        let downloader = S3Downloader::new(reqwest::Client::new(), Default::default(), 100);

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = S3RemoteFile::new(source, source_location);

        let mut destination = tokio::fs::File::create(&target_path).await.unwrap();
        let download_status = downloader
            .download_source("", &file_source, &mut destination)
            .await;

        assert_eq!(download_status, Err(CacheError::NotFound));
    }

    #[tokio::test]
    async fn test_download_invalid_credentials() {
        test::setup();

        let broken_key = S3SourceKey {
            region: S3Region::from("us-east-1"),
            aws_credentials_provider: AwsCredentialsProvider::Static,
            access_key: "".into(),
            secret_key: S3SecretKey("".into()),
        };
        let source = s3_source(broken_key);
        let downloader = S3Downloader::new(reqwest::Client::new(), Default::default(), 100);

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = S3RemoteFile::new(source, source_location);

        let mut destination = tokio::fs::File::create(&target_path).await.unwrap();
        let download_status = downloader
            .download_source("", &file_source, &mut destination)
            .await;

        assert!(
            matches!(download_status, Err(CacheError::PermissionDenied(_))),
            "{download_status:?}"
        );
    }

    #[test]
    fn test_s3_remote_dif_uri() {
        let source_key = Arc::new(S3SourceKey {
            region: S3Region::from("us-east-1"),
            aws_credentials_provider: AwsCredentialsProvider::Static,
            access_key: String::from("abc"),
            secret_key: S3SecretKey("123".into()),
        });
        let source = Arc::new(S3SourceConfig {
            id: SourceId::new("s3-id"),
            bucket: String::from("bucket"),
            prefix: String::from("prefix"),
            source_key,
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        });
        let location = SourceLocation::new("a/key/with spaces");

        let dif = S3RemoteFile::new(source, location);
        assert_eq!(
            dif.uri(),
            RemoteFileUri::new("s3://bucket/prefix/a/key/with%20spaces")
        );
    }
}
