use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::types::Scope;

use super::CacheContents;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub scope: Scope,
    pub time_created: SystemTime,
}

/// A cache entry with optional metadata.
#[derive(Debug, Clone)]
pub struct MdCacheEntry<T> {
    /// Metadata attached to this cache entry.
    pub(crate) metadata: Option<Metadata>,
    /// The cache entry itself.
    pub(crate) contents: CacheContents<T>,
}

impl<T> MdCacheEntry<T> {
    /// Create a cache entry without attached metadata.
    pub(crate) fn without_metadata(contents: CacheContents<T>) -> Self {
        Self {
            metadata: None,
            contents,
        }
    }

    /// Maps a function over this entry's contents.
    pub fn map<U, F>(self, f: F) -> MdCacheEntry<U>
    where
        F: FnOnce(T) -> U,
    {
        MdCacheEntry {
            metadata: self.metadata,
            contents: self.contents.map(f),
        }
    }

    /// Maps a fallible function over this entry's contents.
    pub fn and_then<U, F>(self, f: F) -> MdCacheEntry<U>
    where
        F: FnOnce(T) -> CacheContents<U>,
    {
        MdCacheEntry {
            metadata: self.metadata,
            contents: self.contents.and_then(f),
        }
    }

    /// Returns this entry's metadata.
    pub fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    /// Returns a reference to this entry's contents.
    pub fn contents(&self) -> &CacheContents<T> {
        &self.contents
    }

    /// Consumes this entry and returns the contents.
    pub fn into_contents(self) -> CacheContents<T> {
        self.contents
    }
}
