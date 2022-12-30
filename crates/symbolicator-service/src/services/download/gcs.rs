//! Support to download from Google Cloud Storage buckets.
//!
//! Specifically this supports the [`GcsSourceConfig`] source.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};

use symbolicator_sources::{
    FileType, GcsRemoteFile, GcsSourceConfig, GcsSourceKey, ObjectId, RemoteFile,
};

use crate::cache::CacheEntry;
use crate::utils::gcs::{self, GcsError, GcsToken};

/// An LRU cache for GCS OAuth tokens.
type GcsTokenCache = lru::LruCache<Arc<GcsSourceKey>, Arc<GcsToken>>;

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
        client: reqwest::Client,
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
        if let Some(token) = self.token_cache.lock().unwrap().get(source_key) {
            if !token.is_expired() {
                metric!(counter("source.gcs.token.cached") += 1);
                return Ok(token.clone());
            }
        }

        let source_key = source_key.clone();
        let token = gcs::request_new_token(&self.client, &source_key).await?;
        metric!(counter("source.gcs.token.requests") += 1);
        let token = Arc::new(token);
        self.token_cache
            .lock()
            .unwrap()
            .put(source_key, token.clone());
        Ok(token)
    }

    /// Downloads a source hosted on GCS.
    pub async fn download_source(
        &self,
        file_source: GcsRemoteFile,
        destination: &Path,
    ) -> CacheEntry {
        let key = file_source.key();
        let bucket = file_source.source.bucket.clone();
        tracing::debug!("Fetching from GCS: {} (from {})", &key, bucket);
        let token = self.get_token(&file_source.source.source_key).await?;
        tracing::debug!("Got valid GCS token");

        let url = gcs::download_url(&bucket, &key)?;

        let source = RemoteFile::from(file_source);
        let request = self
            .client
            .get(url.clone())
            .header("authorization", token.bearer_token());

        super::download_reqwest(
            &source,
            request,
            self.connect_timeout,
            self.streaming_timeout,
            destination,
        )
        .await
    }

    pub fn list_files(
        &self,
        source: Arc<GcsSourceConfig>,
        filetypes: &[FileType],
        object_id: &ObjectId,
    ) -> Vec<RemoteFile> {
        super::SourceLocationIter {
            filetypes: filetypes.iter(),
            filters: &source.files.filters,
            object_id,
            layout: source.files.layout,
            next: Vec::new(),
        }
        .map(|loc| GcsRemoteFile::new(source.clone(), loc).into())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use symbolicator_sources::{
        CommonSourceConfig, DirectoryLayoutType, ObjectType, RemoteFileUri, SourceId,
        SourceLocation,
    };

    use crate::cache::CacheError;
    use crate::test;

    use reqwest::Client;
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
        let file_source = GcsRemoteFile::new(source, source_location);

        let download_status = downloader.download_source(file_source, &target_path).await;

        assert!(download_status.is_ok());
        assert!(target_path.exists());

        let hash = Sha1::digest(std::fs::read(target_path).unwrap());
        let hash = format!("{hash:x}");
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
        let file_source = GcsRemoteFile::new(source, source_location);

        let download_status = downloader.download_source(file_source, &target_path).await;

        assert_eq!(download_status, Err(CacheError::NotFound));
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
        let file_source = GcsRemoteFile::new(source, source_location);

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

        let dif = GcsRemoteFile::new(source, location);
        assert_eq!(
            dif.uri(),
            RemoteFileUri::new("gs://bucket/prefix/a/key/with%20spaces")
        );
    }

    // TODO: Test credential caching.
}
