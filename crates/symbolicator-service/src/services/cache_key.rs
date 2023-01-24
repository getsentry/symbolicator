use std::fmt;
use std::path::{Path, PathBuf};

use symbolicator_sources::RemoteFile;

use crate::types::Scope;

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct CacheKey {
    pub cache_key: String,
    pub scope: Scope,
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (scope {})", self.cache_key, self.scope)
    }
}

impl CacheKey {
    /// Creates a [`CacheKey`] for the given [`RemoteFile`] tied to [`Scope`].
    pub fn from_scoped_file(scope: &Scope, file: &RemoteFile) -> Self {
        Self {
            cache_key: file.cache_key(),
            scope: scope.clone(),
        }
    }

    /// Returns the relative path inside the cache for this cache key.
    pub fn relative_path(&self) -> PathBuf {
        let mut path = PathBuf::new();
        path.push(safe_path_segment(self.scope.as_ref()));
        path.push(safe_path_segment(&self.cache_key));
        path
    }

    /// Returns the full cache path for this key inside the provided cache directory.
    pub fn cache_path(&self, cache_dir: &Path, version: u32) -> PathBuf {
        let mut path = PathBuf::from(cache_dir);
        if version != 0 {
            path.push(version.to_string());
        }
        path.push(self.relative_path());
        path
    }
}

/// Protect against:
/// * ".."
/// * absolute paths
/// * ":" (not a threat on POSIX filesystems, but confuses OS X Finder)
fn safe_path_segment(s: &str) -> String {
    s.replace(['.', '/', ':'], "_")
}
