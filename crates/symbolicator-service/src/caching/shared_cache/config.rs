use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemSharedCacheConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcsSharedCacheConfig {
    /// Name of the GCS bucket.
    pub bucket: String,

    /// Optional name of a JSON file containing the service account credentials.
    ///
    /// If this is not provided the JSON will be looked up in the
    /// `GOOGLE_APPLICATION_CREDENTIALS` variable.  Otherwise it is assumed the service is
    /// running since GCP and will retrieve the correct user or service account from the
    /// GCP services.
    #[serde(default)]
    pub service_account_path: Option<PathBuf>,
}

/// The backend to use for the shared cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharedCacheBackendConfig {
    Gcs(GcsSharedCacheConfig),
    Filesystem(FilesystemSharedCacheConfig),
}

/// A remote cache that can be shared between symbolicator instances.
///
/// Any files not in the local cache will be looked up from here before being looked up in
/// their original source.  Additionally derived caches are also stored in here to save
/// computations if another symbolicator has already done the computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedCacheConfig {
    /// The number of allowed concurrent uploads to the shared cache.
    ///
    /// Uploading to the shared cache is not critical for symbolicator's operation and
    /// should not disrupt any normal work it does.  This limits the number of concurrent
    /// uploads so that associated resources are kept in check.
    #[serde(default = "default_max_concurrent_uploads")]
    pub max_concurrent_uploads: usize,

    /// The number of queued up uploads to the cache.
    ///
    /// If more items need to be uploaded to the shared cache than there are allowed
    /// concurrently the uploads will be queued.  If the queue is full the uploads are
    /// simply dropped as they are not critical to symbolicator's operation and not
    /// disrupting symbolicator is more important than uploading to the shared cache.
    #[serde(default = "default_max_upload_queue_size")]
    pub max_upload_queue_size: usize,

    /// The backend to use for the shared cache.
    #[serde(flatten)]
    pub backend: SharedCacheBackendConfig,
}

fn default_max_upload_queue_size() -> usize {
    400
}

fn default_max_concurrent_uploads() -> usize {
    20
}
