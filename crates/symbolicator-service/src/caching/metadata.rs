use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::types::Scope;

use super::{CacheContents, CacheError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub scope: Scope,
    pub time_created: SystemTime,
}

impl Metadata {
    /// Creates metadata for freshly computed data.
    pub(crate) fn fresh_scoped(scope: Scope) -> Self {
        let now = SystemTime::now();
        Self {
            scope,
            time_created: now,
        }
    }
}

/// A cache entry with optional metadata.
#[derive(Debug, Clone)]
pub struct CacheEntry<T> {
    /// Metadata attached to this cache entry.
    pub(crate) metadata: Option<Metadata>,
    /// The cache entry itself.
    pub(crate) contents: CacheContents<T>,
}

impl<T> CacheEntry<T> {
    /// Creates a cache entry from an error, without metadata.
    pub(crate) fn from_err(e: impl Into<CacheError>) -> Self {
        Self {
            metadata: None,
            contents: Err(e.into()),
        }
    }

    /// Maps a function over this entry's contents.
    pub fn map<U, F>(self, f: F) -> CacheEntry<U>
    where
        F: FnOnce(T) -> U,
    {
        CacheEntry {
            metadata: self.metadata,
            contents: self.contents.map(f),
        }
    }

    /// Maps a fallible function over this entry's contents.
    pub fn and_then<U, F>(self, f: F) -> CacheEntry<U>
    where
        F: FnOnce(T) -> CacheContents<U>,
    {
        CacheEntry {
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
