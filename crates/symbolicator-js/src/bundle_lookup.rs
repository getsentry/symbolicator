use std::fmt;
use std::time::Duration;

use symbolicator_service::metric;
use symbolicator_sources::RemoteFileUri;

use crate::interface::ResolvedWith;
use crate::lookup::{CachedFileEntry, FileKey};

type FileInBundleCacheInner =
    moka::sync::Cache<(RemoteFileUri, FileKey), (CachedFileEntry, ResolvedWith)>;

/// A cache that memoizes looking up files in artifact bundles.
#[derive(Clone)]
pub struct FileInBundleCache {
    cache: FileInBundleCacheInner,
}

impl std::fmt::Debug for FileInBundleCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileInBundleCache").finish()
    }
}

impl FileInBundleCache {
    /// Creates a new `FileInBundleCache` with the given `capacity` and an idle time of 1h.
    pub fn new(capacity: u64) -> Self {
        // As all the files are living in memory, we return the size of the contents
        // from the `weigher` which is responsible for this accounting.
        let cache = FileInBundleCacheInner::builder()
            .max_capacity(capacity)
            .time_to_idle(Duration::from_secs(60 * 60))
            .name("file-in-bundle")
            .weigher(|_k, v| {
                let content_size = v
                    .0
                    .entry
                    .as_ref()
                    .map(|cached_file| cached_file.contents.as_deref().unwrap_or_default().len())
                    .unwrap_or_default();
                (std::mem::size_of_val(v) + content_size)
                    .try_into()
                    .unwrap_or(u32::MAX)
            })
            .build();
        Self { cache }
    }

    /// Tries to retrieve a file from the cache.
    ///
    /// We look for the file under `(bundle_uri, key)` for `bundle_uri` in `bundle_uris`.
    /// Retrieval is limited to a specific list of bundles so that e.g. files with the same
    /// `abs_path` belonging to different events are disambiguated.
    pub fn try_get(
        &self,
        bundle_uris: impl Iterator<Item = RemoteFileUri>,
        mut key: FileKey,
    ) -> Option<(RemoteFileUri, CachedFileEntry, ResolvedWith)> {
        for bundle_uri in bundle_uris {
            // XXX: this is a really horrible workaround for not being able to look up things via `(&A, &B)` instead of `&(A, B)`.
            let lookup_key = (bundle_uri, key);
            if let Some((file_entry, resolved_with)) = self.cache.get(&lookup_key) {
                metric!(counter("js.file_in_bundle.hit") += 1);
                return Some((lookup_key.0, file_entry, resolved_with));
            }
            key = lookup_key.1;
        }
        metric!(counter("js.file_in_bundle.miss") += 1);
        None
    }

    /// Inserts `file_entry` into the cache under `(bundle_uri, key)`.
    ///
    /// Files are inserted under a specific bundle so that e.g. files with the same
    /// `abs_path` belonging to different events are disambiguated.
    pub fn insert(
        &self,
        bundle_uri: &RemoteFileUri,
        key: &FileKey,
        resolved_with: ResolvedWith,
        file_entry: &CachedFileEntry,
    ) {
        let mut file_entry = file_entry.clone();
        if matches!(key, FileKey::SourceMap { .. }) {
            if let Ok(cached_file) = file_entry.entry.as_mut() {
                // SourceMaps are usually very large, and we only need them for
                // `sourcemapcache` creation. So do not persist the actual contents here.
                // Rather load them later in `sourcemapcache` generation if needed.
                cached_file.contents = None;
            }
        }
        let key = (bundle_uri.clone(), key.clone());
        self.cache.insert(key, (file_entry, resolved_with))
    }
}
