//! Support to download from Google Cloud Storage buckets.
//!
//! Specifically this supports the [`GcsSourceConfig`] source.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use futures::prelude::*;
use parking_lot::Mutex;
use reqwest::{header, Client, StatusCode};

use symbolicator_sources::{FileType, GcsSourceConfig, GcsSourceKey, ObjectId};

use crate::utils::gcs::{self, request_new_token, GcsError, GcsToken};

use super::locations::SourceLocation;
use super::{content_length_timeout, DownloadError, DownloadStatus, RemoteDif, RemoteDifUri};

/// An LRU cache for GCS OAuth tokens.
type GcsTokenCache = lru::LruCache<Arc<GcsSourceKey>, Arc<GcsToken>>;

/// The GCS-specific [`RemoteDif`].
#[derive(Debug, Clone)]
pub struct GcsRemoteDif {
    pub source: Arc<GcsSourceConfig>,
    pub location: SourceLocation,
}

impl From<GcsRemoteDif> for RemoteDif {
    fn from(source: GcsRemoteDif) -> Self {
        Self::Gcs(source)
    }
}

impl GcsRemoteDif {
    pub fn new(source: Arc<GcsSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the GCS key.
    ///
    /// This is equivalent to the pathname within the bucket.
    pub fn key(&self) -> String {
        self.location.prefix(&self.source.prefix)
    }

    /// Returns the `gs://` URI from which to download this object file.
    pub fn uri(&self) -> RemoteDifUri {
        RemoteDifUri::from_parts("gs", &self.source.bucket, &self.key())
    }
}

/// Downloader implementation that supports the [`GcsSourceConfig`] source.
#[derive(Debug)]
pub struct GcsDownloader {
    token_cache: Mutex<GcsTokenCache>,
    client: reqwest::Client,
    connect_timeout: std::time::Duration,
    streaming_timeout: std::time::Duration,
}

impl GcsDownloader {
    pub fn new(
        client: Client,
        connect_timeout: std::time::Duration,
        streaming_timeout: std::time::Duration,
        token_capacity: NonZeroUsize,
    ) -> Self {
        Self {
            token_cache: Mutex::new(GcsTokenCache::new(token_capacity)),
            client,
            connect_timeout,
            streaming_timeout,
        }
    }

    /// Resolves a valid GCS OAuth token.
    ///
    /// If the cache contains a valid token, then this token is returned. Otherwise, a new token is
    /// requested from GCS and stored in the cache.
    async fn get_token(&self, source_key: &Arc<GcsSourceKey>) -> Result<Arc<GcsToken>, GcsError> {
        if let Some(token) = self.token_cache.lock().get(source_key) {
            if !token.is_expired() {
                metric!(counter("source.gcs.token.cached") += 1);
                return Ok(token.clone());
            }
        }

        let source_key = source_key.clone();
        let token = request_new_token(&self.client, &source_key).await?;
        metric!(counter("source.gcs.token.requests") += 1);
        let token = Arc::new(token);
        self.token_cache.lock().put(source_key, token.clone());
        Ok(token)
    }

    /// Downloads a source hosted on GCS.
    ///
    /// # Directly thrown errors
    /// - [`GcsError::InvalidUrl`]
    /// - [`DownloadError::Reqwest`]
    /// - [`DownloadError::Rejected`]
    /// - [`DownloadError::Canceled`]
    pub async fn download_source(
        &self,
        file_source: GcsRemoteDif,
        destination: &Path,
    ) -> Result<DownloadStatus, DownloadError> {
        let key = file_source.key();
        let bucket = file_source.source.bucket.clone();
        tracing::debug!("Fetching from GCS: {} (from {})", &key, bucket);
        let token = self.get_token(&file_source.source.source_key).await?;
        tracing::debug!("Got valid GCS token");

        let url = gcs::download_url(&bucket, &key)?;

        let source = RemoteDif::from(file_source);
        let request = self
            .client
            .get(url.clone())
            .header("authorization", token.bearer_token())
            .send();
        let request = tokio::time::timeout(self.connect_timeout, request);
        let request = super::measure_download_time(source.source_metric_key(), request);

        let response = request
            .await
            .map_err(|_| DownloadError::Canceled)? // Timeout
            .map_err(|e| {
                tracing::debug!(
                    "Skipping response from GCS {} (from {}): {}",
                    &key,
                    &bucket,
                    &e
                );
                DownloadError::Reqwest(e)
            })?;

        if response.status().is_success() {
            tracing::trace!("Success hitting GCS {} (from {})", &key, bucket);

            let content_length = response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|hv| hv.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok());

            let timeout =
                content_length.map(|cl| content_length_timeout(cl, self.streaming_timeout));
            let stream = response.bytes_stream().map_err(DownloadError::Reqwest);

            super::download_stream(&source, stream, destination, timeout).await
        } else if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
        ) {
            tracing::debug!(
                "Insufficient permissions to download from GCS {} (from {})",
                &key,
                &bucket,
            );
            Ok(DownloadStatus::PermissionDenied)
        // If it's a client error, chances are either it's a 404 or it's permission-related.
        } else if response.status().is_client_error() {
            tracing::debug!(
                "Unexpected client error status code from GCS {} (from {}): {}",
                &key,
                &bucket,
                response.status()
            );
            Ok(DownloadStatus::NotFound)
        } else {
            tracing::debug!(
                "Unexpected status code from GCS {} (from {}): {}",
                &key,
                &bucket,
                response.status()
            );
            Err(DownloadError::Rejected(response.status()))
        }
    }

    pub fn list_files(
        &self,
        source: Arc<GcsSourceConfig>,
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> Vec<RemoteDif> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| GcsRemoteDif::new(source.clone(), loc).into())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::locations::SourceLocation;
    use super::*;

    use symbolicator_sources::{CommonSourceConfig, DirectoryLayoutType, ObjectType, SourceId};

    use crate::test;

    use sha1::{Digest as _, Sha1};

    fn gcs_source(source_key: GcsSourceKey) -> Arc<GcsSourceConfig> {
        Arc::new(GcsSourceConfig {
            id: SourceId::new("gcs-test"),
            bucket: "sentryio-system-symbols-0".to_owned(),
            prefix: "/ios".to_owned(),
            source_key: Arc::new(source_key),
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        })
    }

    #[test]
    fn test_list_files() {
        test::setup();

        let source = gcs_source(test::gcs_source_key!());
        let downloader = GcsDownloader::new(
            Client::new(),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
            100.try_into().unwrap(),
        );

        let object_id = ObjectId {
            code_id: Some("e514c9464eed3be5943a2c61d9241fad".parse().unwrap()),
            code_file: Some("/usr/lib/system/libdyld.dylib".to_owned()),
            debug_id: Some("e514c946-4eed-3be5-943a-2c61d9241fad".parse().unwrap()),
            debug_file: Some("libdyld.dylib".to_owned()),
            object_type: ObjectType::Macho,
        };

        let list = downloader.list_files(source, &[FileType::MachCode], &object_id);
        assert_eq!(list.len(), 1);

        assert!(list[0]
            .uri()
            .to_string()
            .ends_with("e5/14c9464eed3be5943a2c61d9241fad/executable"));
    }

    #[tokio::test]
    async fn test_download_complete() {
        test::setup();

        let source = gcs_source(test::gcs_source_key!());
        let downloader = GcsDownloader::new(
            Client::new(),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
            100.try_into().unwrap(),
        );

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        // Location of /usr/lib/system/libdyld.dylib
        let source_location = SourceLocation::new("e5/14c9464eed3be5943a2c61d9241fad/executable");
        let file_source = GcsRemoteDif::new(source, source_location);

        let download_status = downloader
            .download_source(file_source, &target_path)
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::Completed);
        assert!(target_path.exists());

        let hash = Sha1::digest(std::fs::read(target_path).unwrap());
        let hash = format!("{:x}", hash);
        assert_eq!(hash, "206e63c06da135be1858dde03778caf25f8465b8");
    }

    #[tokio::test]
    async fn test_download_missing() {
        test::setup();

        let source = gcs_source(test::gcs_source_key!());
        let downloader = GcsDownloader::new(
            Client::new(),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
            100.try_into().unwrap(),
        );

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = GcsRemoteDif::new(source, source_location);

        let download_status = downloader
            .download_source(file_source, &target_path)
            .await
            .unwrap();

        assert_eq!(download_status, DownloadStatus::NotFound);
        assert!(!target_path.exists());
    }

    #[tokio::test]
    async fn test_download_invalid_credentials() {
        test::setup();

        let broken_credentials = GcsSourceKey {
            private_key: "".to_owned(),
            client_email: "".to_owned(),
        };

        let source = gcs_source(broken_credentials);
        let downloader = GcsDownloader::new(
            Client::new(),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
            100.try_into().unwrap(),
        );

        let tempdir = test::tempdir();
        let target_path = tempdir.path().join("myfile");

        let source_location = SourceLocation::new("does/not/exist");
        let file_source = GcsRemoteDif::new(source, source_location);

        downloader
            .download_source(file_source, &target_path)
            .await
            .expect_err("authentication should fail");

        assert!(!target_path.exists());
    }

    #[test]
    fn test_gcs_remote_dif_uri() {
        let source_key = Arc::new(GcsSourceKey {
            private_key: String::from("ABC"),
            client_email: String::from("someone@example.com"),
        });
        let source = Arc::new(GcsSourceConfig {
            id: SourceId::new("gcs-id"),
            bucket: String::from("bucket"),
            prefix: String::from("prefix"),
            source_key,
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        });
        let location = SourceLocation::new("a/key/with spaces");

        let dif = GcsRemoteDif::new(source, location);
        assert_eq!(
            dif.uri(),
            RemoteDifUri::new("gs://bucket/prefix/a/key/with%20spaces")
        );
    }

    // TODO: Test credential caching.
}
