use std::fmt::{self, Write};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use symbolicator_sources::RemoteFile;

use crate::caches::{CachePathFormat, CacheVersion};
use crate::types::Scope;

/// The key of an item in an in-memory or on-disk
/// cache.
///
/// Each key belongs to a [Scope], determined by
/// the scope of the symbolication request and the symbol
/// source in question.
#[derive(Debug, Clone, Eq)]
pub struct CacheKey {
    scope: Scope,
    data: Arc<str>,
    hash: [u8; 32],
}

impl CacheKey {
    /// Returns the scope of this cache key.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.cache_path(CacheVersion::new(1234, CachePathFormat::V2))
        )
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
        let mut builder = Self::scoped_builder(scope);
        builder.write_file_meta(file).unwrap();
        builder.build()
    }

    /// Returns the human-readable data that forms the basis of the [`CacheKey`].
    pub fn data(&self) -> &str {
        &self.data
    }

    /// Returns the relative path for this cache key.
    ///
    /// The relative path depends on the `version`'s `path_format`
    /// field. See the documentation of [`CachePathFormat`].
    pub fn cache_path(&self, version: CacheVersion) -> String {
        let CacheVersion {
            number,
            path_format,
        } = version;

        match path_format {
            CachePathFormat::V1 => {
                let mut path = format!("v{number}/{:02x}/", self.hash[0]);
                for b in &self.hash[1..4] {
                    path.write_fmt(format_args!("{b:02x}")).unwrap();
                }
                path.push('/');
                for b in &self.hash[4..] {
                    path.write_fmt(format_args!("{b:02x}")).unwrap();
                }
                path
            }
            CachePathFormat::V2 => {
                let mut path = format!("v{number}/{:02x}/{:02x}/", self.hash[0], self.hash[1]);
                for b in &self.hash[2..] {
                    path.write_fmt(format_args!("{b:02x}")).unwrap();
                }
                path
            }
        }
    }

    /// Create a [`CacheKeyBuilder`] that can be used to build a cache key consisting of all its
    /// contributing sources.
    pub fn scoped_builder(scope: &Scope) -> CacheKeyBuilder {
        let metadata = format!("scope: {scope}\n\n");
        CacheKeyBuilder {
            scope: scope.clone(),
            data: metadata,
        }
    }

    #[cfg(test)]
    pub fn for_testing(scope: Scope, key: impl Into<String>) -> Self {
        let metadata = key.into();

        CacheKeyBuilder {
            scope,
            data: metadata,
        }
        .build()
    }
}

/// A builder for [`CacheKey`]s.
///
/// This builder implements the [`Write`] trait, and the intention of it is to
/// accept human readable, but most importantly **stable**, input.
/// This input is then hashed to form the [`CacheKey`], and can also be serialized alongside
/// the cache files to help debugging.
pub struct CacheKeyBuilder {
    scope: Scope,
    data: String,
}

impl CacheKeyBuilder {
    /// Writes metadata about the [`RemoteFile`] into the [`CacheKey`].
    pub fn write_file_meta(&mut self, file: &RemoteFile) -> Result<(), fmt::Error> {
        self.data.write_fmt(format_args!(
            "source: {}\nlocation: {}\n",
            file.source_id(),
            file.uri()
        ))
    }

    /// Finalize the [`CacheKey`].
    pub fn build(self) -> CacheKey {
        let hash = Sha256::digest(&self.data);

        CacheKey {
            scope: self.scope,
            data: self.data.into(),
            hash: hash.into(),
        }
    }
}

impl fmt::Write for CacheKeyBuilder {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.data.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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

        assert_eq!(
            &key.cache_path(CacheVersion::new(0, CachePathFormat::V1)),
            "v0/f5/e08b92/a55c1357413b5e36547a8b534a014c3a00299e7622e4c4b022a96541"
        );
        assert_eq!(
            &key.cache_path(CacheVersion::new(0, CachePathFormat::V2)),
            "v0/f5/e0/8b92a55c1357413b5e36547a8b534a014c3a00299e7622e4c4b022a96541"
        );
        assert_eq!(
            key.data(),
            "scope: global\n\nsource: foo\nlocation: file://bar.baz\n"
        );

        let built_key = CacheKey::from_scoped_file(&scope, &file);

        assert_eq!(
            built_key.cache_path(CacheVersion::new(0, CachePathFormat::V2)),
            key.cache_path(CacheVersion::new(0, CachePathFormat::V2))
        );

        let mut builder = CacheKey::scoped_builder(&scope);
        builder.write_file_meta(&file).unwrap();

        let location = SourceLocation::new("bar.quux");
        let file = FilesystemRemoteFile::new(source, location).into();
        builder.write_str("\nsecond_source:\n").unwrap();
        builder.write_file_meta(&file).unwrap();
        let key = builder.build();

        assert_eq!(
            &key.cache_path(CacheVersion::new(0, CachePathFormat::V1)),
            "v0/d9/40ba75/07d18c0e9a1d884809670a1e32a72a85ed7563c52909507bf594880a"
        );
        assert_eq!(
            &key.cache_path(CacheVersion::new(0, CachePathFormat::V2)),
            "v0/d9/40/ba7507d18c0e9a1d884809670a1e32a72a85ed7563c52909507bf594880a"
        );
        assert_eq!(
            key.data(),
            "scope: global\n\nsource: foo\nlocation: file://bar.baz\n\nsecond_source:\nsource: foo\nlocation: file://bar.quux\n"
        );
    }
}
