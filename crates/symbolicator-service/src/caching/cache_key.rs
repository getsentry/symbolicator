use std::fmt::{self, Write};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use symbolicator_sources::RemoteFile;

use crate::types::Scope;

#[derive(Debug, Clone, Eq)]
pub struct CacheKey {
    legacy_cache_key: Arc<str>,
    metadata: Arc<str>,
    hash: [u8; 32],
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.cache_path(1234))
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
    /// Creates a [`CacheKey`] for the given [`RemoteFile`] tied to [`Scope`].
    pub fn from_scoped_file(scope: &Scope, file: &RemoteFile) -> Self {
        Self::legacy_builder(scope, file).build()
    }

    /// Returns the human-readable metadata that forms the basis of the [`CacheKey`].
    pub fn metadata(&self) -> &str {
        &self.metadata
    }

    /// Returns the relative path for this cache key.
    ///
    /// The relative path is a sha-256 hash hex-formatted like so:
    /// `v$version/aa/bbccdd/eeff...`
    pub fn cache_path(&self, version: u32) -> String {
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

    /// Creates a stable relative path for the given [`RemoteFile`] tied to [`Scope`].
    ///
    /// This is being used as the "legacy" [`CacheKey`], and also forms the basis for the more
    /// precise modern cache key.
    pub fn relative_scoped_file(scope: &Scope, file: &RemoteFile) -> String {
        let scope = safe_path_segment(scope.as_ref());
        let cache_key = safe_path_segment(&file.cache_key());
        format!("{scope}/{cache_key}")
    }

    /// Create a [`CacheKeyBuilder`] that can be used to build a cache key consisting of all its
    /// contributing sources.
    pub fn legacy_builder(scope: &Scope, file: &RemoteFile) -> CacheKeyBuilder {
        let legacy_cache_key = Self::relative_scoped_file(scope, file);
        let metadata = format!("scope: {scope}\n\n");

        let mut builder = CacheKeyBuilder {
            legacy_cache_key,
            metadata,
        };
        builder.write_file_meta(file).unwrap();
        builder
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
        let metadata = legacy_cache_key.clone();

        CacheKeyBuilder {
            legacy_cache_key,
            metadata,
        }
        .build()
    }
}

/// A builder for [`CacheKey`]s.
///
/// This builder implements the [`Write`](std::fmt::Write) trait, and the intention of it is to
/// accept human readable, but most importantly **stable**, input.
/// This input in then being hashed to form the [`CacheKey`], and can also be serialized alongside
/// the cache files to help debugging.
pub struct CacheKeyBuilder {
    legacy_cache_key: String,
    metadata: String,
}

impl CacheKeyBuilder {
    /// Writes metadata about the [`RemoteFile`] into the [`CacheKey`].
    pub fn write_file_meta(&mut self, file: &RemoteFile) -> Result<(), fmt::Error> {
        self.metadata.write_fmt(format_args!(
            "source: {}\nlocation: {}\n",
            file.source_id(),
            file.uri()
        ))
    }

    /// Finalize the [`CacheKey`].
    pub fn build(self) -> CacheKey {
        let hash = Sha256::digest(&self.metadata);
        // FIXME: `sha2` should really adopt const generics, this is such a pain right now
        let hash = <[u8; 32]>::try_from(hash).expect("sha256 outputs 32 bytes");

        CacheKey {
            legacy_cache_key: self.legacy_cache_key.into(),
            metadata: self.metadata.into(),
            hash,
        }
    }
}

impl fmt::Write for CacheKeyBuilder {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.metadata.write_str(s)
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
            "v0/6f/200788/bd4e6760d55bf6bd50c6d6e98b52379e194f9989fb788b4d37796427"
        );
        assert_eq!(
            key.metadata(),
            "scope: global\n\nsource: foo\nlocation: file:///bar.baz\n"
        );

        let builder = CacheKey::legacy_builder(&scope, &file);
        let built_key = builder.build();

        assert_eq!(built_key.legacy_cache_path(0), key.legacy_cache_path(0));
        assert_eq!(built_key.cache_path(0), key.cache_path(0));

        let mut builder = CacheKey::legacy_builder(&scope, &file);

        let location = SourceLocation::new("bar.quux");
        let file = FilesystemRemoteFile::new(source, location).into();
        builder.write_str("\nsecond_source:\n").unwrap();
        builder.write_file_meta(&file).unwrap();
        let key = builder.build();

        assert_eq!(&key.legacy_cache_path(0), "global/foo_bar_baz");
        assert_eq!(
            &key.cache_path(0),
            "v0/07/e89036/d56878a462eb7949a744afa0a4deb5ed1b7a8154be16f7dd3b220518"
        );
        assert_eq!(
            key.metadata(),
            "scope: global\n\nsource: foo\nlocation: file:///bar.baz\n\nsecond_source:\nsource: foo\nlocation: file:///bar.quux\n"
        );
    }
}
