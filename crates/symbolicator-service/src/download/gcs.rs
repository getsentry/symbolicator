//! Support to download from Google Cloud Storage buckets.

use std::sync::Arc;
use symbolicator_sources::{GcsRemoteFile, GcsSourceAuthorization, GcsSourceKey, GcsSourceToken};

use crate::caching::{CacheContents, CacheError};
use crate::config::DownloadTimeouts;
use crate::utils::gcs::{self, CacheableToken};

use super::Destination;

/// An LRU cache for GCS OAuth tokens.
type GcsTokenCache = moka::future::Cache<Arc<GcsSourceKey>, CacheContents<CacheableToken>>;

/// Downloader implementation that supports the GCS source.
#[derive(Debug)]
pub struct GcsDownloader {
    token_cache: GcsTokenCache,
    client: reqwest::Client,
    timeouts: DownloadTimeouts,
}

impl GcsDownloader {
    pub fn new(client: reqwest::Client, timeouts: DownloadTimeouts, token_capacity: u64) -> Self {
        Self {
            token_cache: GcsTokenCache::builder()
                .max_capacity(token_capacity)
                .build(),
            client,
            timeouts,
        }
    }

    /// Resolves a valid GCS OAuth token.
    ///
    /// If the cache contains a valid token, then this token is returned. Otherwise, a new token is
    /// requested from GCS and stored in the cache.
    async fn get_token(
        &self,
        source_key: &GcsSourceAuthorization,
    ) -> CacheContents<GcsSourceToken> {
        metric!(counter("source.gcs.token.access") += 1);

        match source_key {
            GcsSourceAuthorization::SourceKey(source_key) => {
                let init = Box::pin(async {
                    metric!(counter("source.gcs.token.computation") += 1);
                    let token = gcs::request_new_token(&self.client, source_key).await;
                    token.map_err(CacheError::from)
                });
                let replace_if = |entry: &CacheContents<CacheableToken>| {
                    entry.as_ref().map_or(true, |t| t.is_expired())
                };

                self.token_cache
                    .entry_by_ref(source_key)
                    .or_insert_with_if(init, replace_if)
                    .await
                    .into_value()
                    .map(|token| GcsSourceToken {
                        bearer_token: token.bearer_token().clone(),
                    })
            }
            GcsSourceAuthorization::SourceToken(source_token) => Ok(source_token.clone()),
        }
    }

    /// Downloads a source hosted on GCS.
    pub async fn download_source(
        &self,
        source_name: &str,
        file_source: &GcsRemoteFile,
        destination: impl Destination,
    ) -> CacheContents {
        let key = file_source.key();
        let bucket = &file_source.source.bucket;
        tracing::debug!("Fetching from GCS: {} (from {})", key, bucket);
        let token = self
            .get_token(&file_source.source.source_authorization)
            .await?;
        tracing::debug!("Got valid GCS token");

        let url = gcs::download_url(bucket, &key)?;

        let builder = self
            .client
            .get(url)
            .header("authorization", format!("Bearer {}", token.bearer_token.0));

        super::download_reqwest(
            source_name,
            builder,
            &self.timeouts,
            destination,
            &super::GenericErrorHandler,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use symbolicator_sources::{
        CommonSourceConfig, DirectoryLayoutType, GcsBearerToken, GcsPrivateKey, GcsSourceConfig,
        GcsSourceToken, RemoteFileUri, SourceId, SourceLocation,
    };

    use crate::test;

    use reqwest::Client;
    use sha1::{Digest as _, Sha1};

    fn gcs_source(source_key: GcsSourceKey) -> Arc<GcsSourceConfig> {
        Arc::new(GcsSourceConfig {
            id: SourceId::new("gcs-test"),
            bucket: "sentryio-system-symbols-0-test".to_owned(),
            prefix: "/ios".to_owned(),
            source_authorization: GcsSourceAuthorization::SourceKey(Arc::new(source_key)),
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        })
    }

    #[tokio::test]
    async fn test_download_complete() {
        test::setup();

        let source = gcs_source(test::gcs_source_key!());
        let downloader =
            GcsDownloader::new(Client::new(), Default::default(), 100.try_into().unwrap());

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        // Location of /usr/lib/system/libdyld.dylib
        let source_location = SourceLocation::new("e5/14c9464eed3be5943a2c61d9241fad/executable");
        let file_source = GcsRemoteFile::new(source, source_location);

        let mut destination = tokio::fs::File::create(&target_path).await.unwrap();
        let download_status = downloader
            .download_source("", &file_source, &mut destination)
            .await;

        assert!(download_status.is_ok());
        assert!(target_path.exists());

        let hash = Sha1::digest(std::fs::read(target_path).unwrap());
        let hash = format!("{hash:x}");
        assert_eq!(hash, "206e63c06da135be1858dde03778caf25f8465b8");
    }

    #[tokio::test]
    async fn test_use_token_from_request() {
        let auth = GcsSourceAuthorization::SourceToken(GcsSourceToken {
            bearer_token: GcsBearerToken("this-is-a-secret-token".into()),
        });
        let downloader =
            GcsDownloader::new(Client::new(), Default::default(), 100.try_into().unwrap());
        let token = downloader.get_token(&auth).await.unwrap();
        assert_eq!(token.bearer_token.0.as_ref(), "this-is-a-secret-token");
    }

    #[tokio::test]
    async fn test_download_missing() {
        test::setup();

        let source = gcs_source(test::gcs_source_key!());
        let downloader =
            GcsDownloader::new(Client::new(), Default::default(), 100.try_into().unwrap());

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = GcsRemoteFile::new(source, source_location);

        let mut destination = tokio::fs::File::create(&target_path).await.unwrap();
        let download_status = downloader
            .download_source("", &file_source, &mut destination)
            .await;

        assert_eq!(download_status, Err(CacheError::NotFound));
    }

    #[tokio::test]
    async fn test_download_invalid_credentials() {
        test::setup();

        let broken_credentials = GcsSourceKey {
            private_key: GcsPrivateKey("".to_owned().into()),
            client_email: "".to_owned(),
        };

        let source = gcs_source(broken_credentials);
        let downloader =
            GcsDownloader::new(Client::new(), Default::default(), 100.try_into().unwrap());

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = GcsRemoteFile::new(source, source_location);

        let mut destination = tokio::fs::File::create(&target_path).await.unwrap();
        downloader
            .download_source("", &file_source, &mut destination)
            .await
            .expect_err("authentication should fail");
    }

    #[test]
    fn test_gcs_remote_dif_uri() {
        let source_key = GcsSourceAuthorization::SourceKey(Arc::new(GcsSourceKey {
            private_key: GcsPrivateKey("ABC".to_owned().into()),
            client_email: String::from("someone@example.com"),
        }));
        let source = Arc::new(GcsSourceConfig {
            id: SourceId::new("gcs-id"),
            bucket: String::from("bucket"),
            prefix: String::from("prefix"),
            source_authorization: source_key,
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        });
        let location = SourceLocation::new("a/key/with spaces");

        let dif = GcsRemoteFile::new(source, location);
        assert_eq!(
            dif.uri(),
            RemoteFileUri::new("gs://bucket/prefix/a/key/with%20spaces")
        );
    }

    // TODO: Test credential caching.
}
