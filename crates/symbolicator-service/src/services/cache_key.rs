use std::fmt;
use std::path::{Path, PathBuf};

use symbolicator_sources::RemoteFile;

use crate::types::Scope;

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct CacheKey {
    pub cache_key: String,
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.cache_key)
    }
}

impl CacheKey {
    /// Creates a [`CacheKey`] for the given [`RemoteFile`] tied to [`Scope`].
    pub fn from_scoped_file(scope: &Scope, file: &RemoteFile) -> Self {
        let scope = safe_path_segment(scope.as_ref());
        let cache_key = safe_path_segment(&file.cache_key());
        let cache_key = format!("{scope}/{cache_key}");
        Self { cache_key }
    }

    /// Returns the relative path inside the cache for this cache key.
    pub fn relative_path(&self) -> PathBuf {
        let mut path = PathBuf::new();
        path.push(&self.cache_key);
        path
    }

    /// Returns the full cache path for this key inside the provided cache directory.
    pub fn cache_path(&self, cache_dir: &Path, version: u32) -> PathBuf {
        let mut path = PathBuf::from(cache_dir);
        if version != 0 {
            path.push(version.to_string());
        }
        path.push(&self.cache_key);
        path
    }
}

/// Protect against:
/// * ".."
/// * absolute paths
/// * ":" (not a threat on POSIX filesystems, but confuses OS X Finder)
fn safe_path_segment(s: &str) -> String {
    s.replace(['.', '/', '\\', ':'], "_")
}
