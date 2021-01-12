//! Implementations for the types describing DIF object files.

use std::fmt;

use crate::cache::CacheStatus;
use crate::sources::SourceId;

use super::{
    AllObjectCandidates, Arch, CodeId, CompleteObjectInfo, DebugId, ObjectCandidate,
    ObjectFeatures, ObjectFileSourceURI, ObjectFileStatus, ObjectUseInfo, RawObjectInfo,
};

impl Default for ObjectUseInfo {
    fn default() -> Self {
        Self::None
    }
}

impl ObjectUseInfo {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

impl From<Vec<ObjectCandidate>> for AllObjectCandidates {
    fn from(mut source: Vec<ObjectCandidate>) -> Self {
        source
            .sort_by_cached_key(|candidate| (candidate.source.clone(), candidate.location.clone()));
        Self(source)
    }
}

impl AllObjectCandidates {
    /// Sets the [`ObjectCandidate::debug`] field for the specified DIF object.
    ///
    /// You can only request symcaches from a DIF object that was already in the metadata
    /// candidate list, therefore if the candidate is missing it is treated as an error.
    pub fn set_debug(
        &mut self,
        source: SourceId,
        location: &ObjectFileSourceURI,
        info: ObjectUseInfo,
    ) {
        let found_pos = self.0.binary_search_by(|candidate| {
            candidate
                .source
                .cmp(&source)
                .then(candidate.location.cmp(location))
        });
        match found_pos {
            Ok(index) => {
                if let Some(mut candidate) = self.0.get_mut(index) {
                    candidate.debug = info;
                }
            }
            Err(_) => {
                sentry::capture_message(
                    "Missing ObjectCandidate in AllObjectCandidates::set_debug",
                    sentry::Level::Error,
                );
            }
        }
    }

    /// Sets the [`ObjectCandidate::unwind`] field for the specified DIF object.
    ///
    /// You can only request cficaches from a DIF object that was already in the metadata
    /// candidate list, therefore if the candidate is missing it is treated as an error.
    pub fn set_unwind(
        &mut self,
        source: SourceId,
        location: &ObjectFileSourceURI,
        info: ObjectUseInfo,
    ) {
        let found_pos = self.0.binary_search_by(|candidate| {
            candidate
                .source
                .cmp(&source)
                .then(candidate.location.cmp(location))
        });
        match found_pos {
            Ok(index) => {
                if let Some(mut candidate) = self.0.get_mut(index) {
                    candidate.unwind = info;
                }
            }
            Err(_) => {
                sentry::capture_message(
                    "Missing ObjectCandidate in AllObjectCandidates::set_unwind",
                    sentry::Level::Error,
                );
            }
        }
    }

    /// Returns `true` if the collections contains no [`ObjectCandidate`]s.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Removes all DIF object from this candidates collection.
    pub fn clear(&mut self) {
        self.0.clear()
    }

    /// Merge in the other collection of candidates.
    ///
    /// If a candidate already existed in the collection all data which is present in both
    /// will be overwritten by the data in `other`.  Practically that means
    /// [`ObjectCandidate::download`] will be overwritten by `other` and for
    /// [`ObjectCandidate::unwind`] and [`ObjectCandidate::debug`] it will be overwritten by
    /// `other` if they are not [`ObjectUseInfo::None`].
    pub fn merge(&mut self, other: AllObjectCandidates) {
        for other_info in other.0 {
            let found_pos = self.0.binary_search_by(|candidate| {
                candidate
                    .source
                    .cmp(&other_info.source)
                    .then(candidate.location.cmp(&other_info.location))
            });
            match found_pos {
                Ok(index) => {
                    if let Some(mut info) = self.0.get_mut(index) {
                        info.download = other_info.download;
                        if other_info.unwind != ObjectUseInfo::None {
                            info.unwind = other_info.unwind;
                        }
                        if other_info.debug != ObjectUseInfo::None {
                            info.debug = other_info.debug;
                        }
                    }
                }
                Err(index) => {
                    self.0.insert(index, other_info);
                }
            }
        }
    }
}

impl From<RawObjectInfo> for CompleteObjectInfo {
    fn from(mut raw: RawObjectInfo) -> Self {
        raw.debug_id = raw
            .debug_id
            .filter(|id| !id.is_empty())
            .and_then(|id| id.parse::<DebugId>().ok())
            .map(|id| id.to_string());

        raw.code_id = raw
            .code_id
            .filter(|id| !id.is_empty())
            .and_then(|id| id.parse::<CodeId>().ok())
            .map(|id| id.to_string());

        CompleteObjectInfo {
            debug_status: ObjectFileStatus::Unused,
            unwind_status: None,
            features: ObjectFeatures::default(),
            arch: Arch::Unknown,
            raw,
            candidates: AllObjectCandidates::default(),
        }
    }
}

impl ObjectUseInfo {
    /// Construct [`ObjectUseInfo`] for an object from a derived cache.
    ///
    /// The [`ObjectUseInfo`] provides information about items stored in a cache and which
    /// are derived from an original object cache: the [`symcaches`] and the [`cficaches`].
    /// These caches have an edge case where if the underlying cache thought the object was
    /// there but now it could not be fetched again.  This is converted to an error case.
    ///
    /// [`symcaches`]: crate::actors::symcaches
    /// [`cficaches`]: crate::actors::cficaches
    pub fn from_derived_status(derived: CacheStatus, original: CacheStatus) -> Self {
        match derived {
            CacheStatus::Positive => ObjectUseInfo::Ok,
            CacheStatus::Negative => {
                if original == CacheStatus::Positive {
                    ObjectUseInfo::Error {
                        details: String::from("Object file no longer available"),
                    }
                } else {
                    // If the original cache was already missing then it will already be
                    // reported and we do not want to report anything.
                    ObjectUseInfo::None
                }
            }
            CacheStatus::Malformed => ObjectUseInfo::Malformed,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::ObjectDownloadInfo;

    use super::*;

    #[test]
    fn test_all_object_info_merge_insert_new() {
        // If a candidate didn't exist yet it should be inserted in order.
        let src_a = ObjectCandidate {
            source: SourceId::new("A"),
            location: ObjectFileSourceURI::new("a"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::Ok,
        };
        let src_b = ObjectCandidate {
            source: SourceId::new("B"),
            location: ObjectFileSourceURI::new("b"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::Ok,
        };
        let src_c = ObjectCandidate {
            source: SourceId::new("C"),
            location: ObjectFileSourceURI::new("c"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::Ok,
        };

        let mut all: AllObjectCandidates = vec![src_a, src_c].into();
        let other: AllObjectCandidates = vec![src_b].into();
        all.merge(other);
        assert_eq!(all.0[0].source, SourceId::new("A"));
        assert_eq!(all.0[1].source, SourceId::new("B"));
        assert_eq!(all.0[2].source, SourceId::new("C"));
    }

    #[test]
    fn test_all_object_info_merge_overwrite() {
        let src0 = ObjectCandidate {
            source: SourceId::new("A"),
            location: ObjectFileSourceURI::new("a"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::None,
        };
        let src1 = ObjectCandidate {
            source: SourceId::new("A"),
            location: ObjectFileSourceURI::new("a"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Malformed,
            debug: ObjectUseInfo::Ok,
        };

        let mut all: AllObjectCandidates = vec![src0].into();
        assert_eq!(all.0[0].unwind, ObjectUseInfo::Ok);
        assert_eq!(all.0[0].debug, ObjectUseInfo::None);

        let other: AllObjectCandidates = vec![src1].into();
        all.merge(other);
        assert_eq!(all.0[0].unwind, ObjectUseInfo::Malformed);
        assert_eq!(all.0[0].debug, ObjectUseInfo::Ok);
    }

    #[test]
    fn test_all_object_info_merge_no_overwrite() {
        let src0 = ObjectCandidate {
            source: SourceId::new("A"),
            location: ObjectFileSourceURI::new("uri://dummy"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::Ok,
        };
        let src1 = ObjectCandidate {
            source: SourceId::new("A"),
            location: ObjectFileSourceURI::new("uri://dummy"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::None,
            debug: ObjectUseInfo::None,
        };

        let mut all: AllObjectCandidates = vec![src0].into();
        assert_eq!(all.0[0].unwind, ObjectUseInfo::Ok);
        assert_eq!(all.0[0].debug, ObjectUseInfo::Ok);

        let other: AllObjectCandidates = vec![src1].into();
        all.merge(other);
        assert_eq!(all.0[0].unwind, ObjectUseInfo::Ok);
        assert_eq!(all.0[0].debug, ObjectUseInfo::Ok);
    }
}

impl ObjectFileSourceURI {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl<T> From<T> for ObjectFileSourceURI
where
    T: Into<String>,
{
    fn from(source: T) -> Self {
        Self(source.into())
    }
}

impl fmt::Display for ObjectFileSourceURI {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
