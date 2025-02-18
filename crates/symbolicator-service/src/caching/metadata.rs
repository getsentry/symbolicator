use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::types::Scope;

use super::{CacheContents, CacheError};

/// Metadata associated with a cache entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// The scope of the cache entry. This is either a project ID
    /// or [`Scope::Global`] if the item came from a public source.
    pub scope: Scope,
    /// The time this cache entry was created.
    ///
    /// This is serialized with second precision.
    #[serde(with = "timestamp")]
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

/// Module for second-precision [`SystemTime`] de/serialization.
mod timestamp {
    use std::time::{Duration, SystemTime};

    use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serializer};

    /// Serializes a [`SystemTime`] as seconds since the UNIX epoch.
    pub(super) fn serialize<S>(timestamp: &SystemTime, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let Ok(since_epoch) = timestamp.duration_since(SystemTime::UNIX_EPOCH) else {
            // copied from the impl of `Serialize` for `SystemTime`
            return Err(S::Error::custom("SystemTime must be later than UNIX_EPOCH"));
        };
        s.serialize_u64(since_epoch.as_secs())
    }

    /// Deserializes a [`SystemTime`] from seconds since the UNIX epoch.
    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(secs))
            // copied from the impl of `Deserialize` for `SystemTime`
            .ok_or_else(|| D::Error::custom("overflow deserializing SystemTime"))
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use crate::types::Scope;

    use super::Metadata;

    /// Serializing and deserializing a timestamp should lose
    /// at most one second.
    #[test]
    fn timestamp_serialization_delta() {
        let now = SystemTime::now();

        let metadata = Metadata {
            scope: Scope::Global,
            time_created: now,
        };

        let serialized = serde_json::to_string(&metadata).unwrap();
        let deserialized: Metadata = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.scope, metadata.scope);
        let time_delta = metadata
            .time_created
            .duration_since(deserialized.time_created)
            .unwrap();
        assert!(time_delta < Duration::from_secs(1));
    }
}
