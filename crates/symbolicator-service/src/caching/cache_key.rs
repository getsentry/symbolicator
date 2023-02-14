use std::fmt;

use sha2::{Digest, Sha256};
use symbolicator_sources::RemoteFile;

use crate::types::Scope;

#[derive(Debug, Clone, Eq)]
pub struct CacheKey {
    legacy_cache_key: String,
    hash: [u8; 32],
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.legacy_cache_key)
    }
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl std::hash::Hash for CacheKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

impl CacheKey {
    /// Creates a stable relative path for the given [`RemoteFile`] tied to [`Scope`].
    ///
    /// This is being used as the "legacy" [`CacheKey`], and also forms the basis for the more
    /// precise modern cache key.
    pub fn relative_scoped_file(scope: &Scope, file: &RemoteFile) -> String {
        let scope = safe_path_segment(scope.as_ref());
        let cache_key = safe_path_segment(&file.cache_key());
        format!("{scope}/{cache_key}")
    }

    /// Creates a [`CacheKey`] for the given [`RemoteFile`] tied to [`Scope`].
    pub fn from_scoped_file(scope: &Scope, file: &RemoteFile) -> Self {
        Self::builder(scope, file).build()
    }

    /// Create a [`CacheKeyBuilder`] that can be used to build a cache key consisting of all its
    /// contributing sources.
    pub fn builder(scope: &Scope, file: &RemoteFile) -> CacheKeyBuilder {
        let legacy_cache_key = Self::relative_scoped_file(scope, file);
        let mut hasher = Sha256::new();
        hasher.update(&legacy_cache_key);

        CacheKeyBuilder {
            legacy_cache_key,
            hasher,
        }
    }

    /// Returns the relative path for this cache key.
    ///
    /// The relative path is a sha-256 hash hex-formatted like so:
    /// `v$version/aa/bbccdd/eeff...`
    pub fn cache_path(&self, version: u32) -> String {
        use std::fmt::Write;

        let mut path = format!("v{version}/{:02x}/", self.hash[0]);
        for b in &self.hash[1..4] {
            path.write_fmt(format_args!("{b:02x}")).unwrap();
        }
        path.push('/');
        for b in &self.hash[4..] {
            path.write_fmt(format_args!("{b:02x}")).unwrap();
        }
        path
    }

    /// Returns the full cache path for this key inside the provided cache directory.
    pub fn legacy_cache_path(&self, version: u32) -> String {
        let mut path = if version != 0 {
            format!("{version}/")
        } else {
            String::new()
        };
        path.push_str(&self.legacy_cache_key);
        path
    }

    #[cfg(test)]
    pub fn for_testing(key: impl Into<String>) -> Self {
        let legacy_cache_key = key.into();
        let mut hasher = Sha256::new();
        hasher.update(&legacy_cache_key);

        CacheKeyBuilder {
            legacy_cache_key,
            hasher,
        }
        .build()
    }
}

pub struct CacheKeyBuilder {
    legacy_cache_key: String,
    hasher: Sha256,
}

impl CacheKeyBuilder {
    /// Update the Cache Key hash with the provided `data`.
    pub fn update(&mut self, data: impl AsRef<[u8]>) {
        self.hasher.update(data);
    }

    /// Finalize the [`CacheKey`].
    pub fn build(self) -> CacheKey {
        // FIXME: `sha2` should really adopt const generics, this is such a pain right now
        let hash = self.hasher.finalize();
        let hash = <[u8; 32]>::try_from(hash).expect("sha256 outputs 32 bytes");

        CacheKey {
            legacy_cache_key: self.legacy_cache_key,
            hash,
        }
    }
}

/// Protect against:
/// * ".."
/// * absolute paths
/// * ":" (not a threat on POSIX filesystems, but confuses OS X Finder)
fn safe_path_segment(s: &str) -> String {
    s.replace(['.', '/', '\\', ':'], "_")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use symbolicator_sources::{
        FilesystemRemoteFile, FilesystemSourceConfig, SourceId, SourceLocation,
    };

    use super::*;

    #[test]
    fn test_hashed_paths() {
        let scope = Scope::Global;
        let source = Arc::new(FilesystemSourceConfig {
            id: SourceId::new("foo"),
            path: PathBuf::new(),
            files: Default::default(),
        });
        let location = SourceLocation::new("bar.baz");
        let file = FilesystemRemoteFile::new(source.clone(), location).into();

        let key = CacheKey::from_scoped_file(&scope, &file);

        assert_eq!(&key.legacy_cache_path(0), "global/foo_bar_baz");
        assert_eq!(&key.legacy_cache_path(1), "1/global/foo_bar_baz");
        assert_eq!(
            &key.cache_path(0),
            "v0/95/ce2928/758cb655460741a6599f934db2b6186b298a58473cf61f8e0c7c8c4f"
        );

        let builder = CacheKey::builder(&scope, &file);
        let built_key = builder.build();

        assert_eq!(built_key.legacy_cache_path(0), key.legacy_cache_path(0));
        assert_eq!(built_key.cache_path(0), key.cache_path(0));

        let mut builder = CacheKey::builder(&scope, &file);

        let location = SourceLocation::new("bar.quux");
        let file = FilesystemRemoteFile::new(source, location).into();
        builder.update("second_source");
        builder.update(CacheKey::relative_scoped_file(&scope, &file));
        let key = builder.build();

        assert_eq!(&key.legacy_cache_path(0), "global/foo_bar_baz");
        assert_eq!(
            &key.cache_path(0),
            "v0/ee/0bab60/b041688910474e611fc8b9cb46b8f72bac1864b02e79d9493cf1d165"
        );
    }
}
