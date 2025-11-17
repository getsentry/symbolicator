//! Implementations for the types describing DIF object files.

use serde::{Deserialize, Serialize};

use symbolicator_sources::{RemoteFileUri, SourceId};

/// The VCS type extracted from PDB SRCSRV streams.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SrcSrvVcs {
    /// Git version control system.
    Git,
    /// Perforce version control system.
    Perforce,
    /// Other or unknown VCS.
    Other,
}

impl SrcSrvVcs {
    /// Parse a VCS name string into a SrcSrvVcs enum.
    pub fn from_vcs_name(vcs_name: &str) -> Self {
        let vcs_lower = vcs_name.to_lowercase();
        if vcs_lower.contains("git") {
            Self::Git
        } else if vcs_lower.contains("perforce") {
            Self::Perforce
        } else {
            Self::Other
        }
    }

    /// Get the string representation for metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Perforce => "perforce",
            Self::Other => "other",
        }
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObjectFeatures {
    /// The object file contains full debug info.
    pub has_debug_info: bool,

    /// The object file contains unwind info.
    pub has_unwind_info: bool,

    /// The object file contains a symbol table.
    pub has_symbols: bool,

    /// The object file had sources available.
    #[serde(default)]
    pub has_sources: bool,

    /// The SRCSRV VCS type, if available.
    /// This is extracted from PDB files that contain SRCSRV streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srcsrv_vcs: Option<SrcSrvVcs>,
}

impl ObjectFeatures {
    pub fn merge(&mut self, other: ObjectFeatures) {
        self.has_debug_info |= other.has_debug_info;
        self.has_unwind_info |= other.has_unwind_info;
        self.has_symbols |= other.has_symbols;
        self.has_sources |= other.has_sources;
        // Keep the first non-None srcsrv_vcs value
        if self.srcsrv_vcs.is_none() {
            self.srcsrv_vcs = other.srcsrv_vcs;
        }
    }
}

/// Information about a Debug Information File in the `CompleteObjectInfo`.
///
/// All DIFs are backed by an [`ObjectHandle`](crate::objects::ObjectHandle).  But we
/// may not have been able to get hold of this object file.  We still want to describe the
/// relevant DIF however.
///
/// Currently has no [`ObjectId`] attached and the parent container is expected to know
/// which ID this DIF info was for.
///
/// [`ObjectId`]: symbolicator_sources::ObjectId
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectCandidate {
    /// The ID of the object source where this DIF was expected to be found.
    ///
    /// This refers back to the IDs of sources in the symbolication requests, as well as any
    /// globally configured sources from symbolicator's configuration.
    ///
    /// Generally this is a short readable string.
    pub source: SourceId,
    /// The location of this DIF on the object source.
    ///
    /// This is generally a URI which makes sense for the source type, however no guarantees
    /// are given and it could be any string.
    pub location: RemoteFileUri,
    /// Information about fetching or downloading this DIF object.
    ///
    /// This section is always present and will at least have a `status` field.
    pub download: ObjectDownloadInfo,
    /// Information about any unwind info in this DIF object.
    ///
    /// This section is only present if this DIF object was used for unwinding by the
    /// symbolication request.
    #[serde(skip_serializing_if = "ObjectUseInfo::is_none", default)]
    pub unwind: ObjectUseInfo,
    /// Information about any debug info this DIF object may have.
    ///
    /// This section is only present if this DIF object was used for symbol lookups by the
    /// symbolication request.
    #[serde(skip_serializing_if = "ObjectUseInfo::is_none", default)]
    pub debug: ObjectUseInfo,
}

/// Information about downloading of a DIF object.
///
/// This is part of the larger [`ObjectCandidate`] struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ObjectDownloadInfo {
    /// The DIF object was downloaded successfully.
    ///
    /// The `features` field describes which [`ObjectFeatures`] the object is expected to
    /// provide, though whether these are actually usable has not yet been verified.
    Ok { features: ObjectFeatures },
    /// The DIF object could not be parsed after downloading.
    ///
    /// This is only a basic validity check of whether the container of the object file can
    /// be parsed.  Actually using the object for CFI or symbols might result in more
    /// detailed problems, see [`ObjectUseInfo`] for more on this.
    Malformed,
    /// Symbolicator had insufficient permissions to download the DIF object.
    ///
    /// More details should be available in the `details` field, which is not meant to be
    /// machine parsable.
    NoPerm { details: String },
    /// The DIF object was not found.
    ///
    /// This is considered a *regular notfound* where the object was simply not available at
    /// the source expected to provde this DIF.  Thus no further details are available.
    NotFound,
    /// An error occurred during downloading of this DIF object.
    ///
    /// This is mostly an internal error from symbolicator which is considered transient.
    /// The next attempt to access this DIF object will retry the download.
    ///
    /// More details should be available in the `details` field, which is not meant to be
    /// machine parsable.
    Error { details: String },
}

/// Information about the use of a DIF object.
///
/// This information is applicable to both "unwind" and "debug" use cases, in each case the
/// object needs to be processed a little more than just the downloaded artifact and we may
/// need to report some status on this.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ObjectUseInfo {
    /// The DIF object was successfully used to provide the required information.
    ///
    /// This means the object was used for CFI when used for [`ObjectCandidate::unwind`]
    Ok,
    /// The DIF object contained malformed data which could not be used.
    Malformed,
    /// An error occurred when attempting to use this DIF object.
    ///
    /// This is mostly an internal error from symbolicator which is considered transient.
    /// The next attempt to access this DIF object will retry using this DIF object.
    ///
    /// More details should be available in the `details` field, which is not meant to be
    /// machine parsable.
    Error { details: String },
    /// Internal state, this is not serialised.
    ///
    /// This enum is not serialised into its parent object when it is set to this value.
    #[default]
    None,
}

impl ObjectUseInfo {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// The candidate cache status that we want to set
#[derive(Eq, PartialEq)]
pub enum CandidateStatus {
    Debug,
    Unwind,
    None,
}

/// Newtype around a collection of [`ObjectCandidate`] structs.
///
/// This abstracts away some common operations needed on this collection.
///
/// Invariant: The `ObjectCandidate`s contained in this collection are always
/// sorted and unique by `(source, location)`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct AllObjectCandidates(Vec<ObjectCandidate>);

impl AllObjectCandidates {
    /// Sets the `debug` or `unwind` status field for the specified DIF object.
    ///
    /// You can only request derived caches from a DIF object that was already in the metadata
    /// candidate list, therefore if the candidate is missing it is treated as an error.
    pub fn set_status(
        &mut self,
        candidate_status: CandidateStatus,
        source: &SourceId,
        uri: &RemoteFileUri,
        info: ObjectUseInfo,
    ) {
        if candidate_status == CandidateStatus::None {
            return;
        }

        let found_pos = self.0.binary_search_by_key(&(source, uri), |candidate| {
            (&candidate.source, &candidate.location)
        });
        match found_pos {
            Ok(index) => {
                if let Some(candidate) = self.0.get_mut(index) {
                    match candidate_status {
                        CandidateStatus::Debug => candidate.debug = info,
                        CandidateStatus::Unwind => candidate.unwind = info,
                        _ => unreachable!(),
                    };
                }
            }
            Err(_) => {
                sentry::capture_message(
                    "Missing ObjectCandidate in AllObjectCandidates::set_status",
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
    pub fn merge(&mut self, other: &AllObjectCandidates) {
        for other_info in &other.0 {
            self.merge_one(other_info);
        }
    }

    /// Merges another candidate into this collection.
    ///
    /// If `candidate` already existed in the collection all data which is present in both
    /// will be overwritten by the data in `candidate`.  Practically that means
    /// [`ObjectCandidate::download`] will be overwritten by `candidate` and for
    /// [`ObjectCandidate::unwind`] and [`ObjectCandidate::debug`] it will be overwritten by
    /// `candidate` if they are not [`ObjectUseInfo::None`].
    pub fn merge_one(&mut self, candidate: &ObjectCandidate) {
        let key = (&candidate.source, &candidate.location);
        let found_pos = self
            .0
            .binary_search_by_key(&key, |candidate| (&candidate.source, &candidate.location));
        match found_pos {
            Ok(index) => {
                if let Some(info) = self.0.get_mut(index) {
                    info.download = candidate.download.clone();
                    if candidate.unwind != ObjectUseInfo::None {
                        info.unwind = candidate.unwind.clone();
                    }
                    if candidate.debug != ObjectUseInfo::None {
                        info.debug = candidate.debug.clone();
                    }
                }
            }
            Err(index) => {
                self.0.insert(index, candidate.clone());
            }
        }
    }

    /// Returns the vector of `ObjectCandidate`s backing this collection.
    pub fn into_inner(self) -> Vec<ObjectCandidate> {
        self.0
    }

    /// Returns an iterator over the [`ObjectCandidates`](ObjectCandidate)
    /// in this collection.
    pub fn iter(&self) -> std::slice::Iter<'_, ObjectCandidate> {
        self.0.as_slice().iter()
    }
}

impl From<Vec<ObjectCandidate>> for AllObjectCandidates {
    fn from(mut source: Vec<ObjectCandidate>) -> Self {
        source
            .sort_by_cached_key(|candidate| (candidate.source.clone(), candidate.location.clone()));
        source.dedup_by_key(|candidate| (candidate.source.clone(), candidate.location.clone()));
        Self(source)
    }
}

impl<'a> IntoIterator for &'a AllObjectCandidates {
    type Item = &'a ObjectCandidate;

    type IntoIter = std::slice::Iter<'a, ObjectCandidate>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_object_info_merge_insert_new() {
        // If a candidate didn't exist yet it should be inserted in order.
        let src_a = ObjectCandidate {
            source: SourceId::new("A"),
            location: RemoteFileUri::new("a"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::Ok,
        };
        let src_b = ObjectCandidate {
            source: SourceId::new("B"),
            location: RemoteFileUri::new("b"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::Ok,
        };
        let src_c = ObjectCandidate {
            source: SourceId::new("C"),
            location: RemoteFileUri::new("c"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::Ok,
        };

        let mut all: AllObjectCandidates = vec![src_a, src_c].into();
        let other: AllObjectCandidates = vec![src_b].into();
        all.merge(&other);
        assert_eq!(all.0[0].source, SourceId::new("A"));
        assert_eq!(all.0[1].source, SourceId::new("B"));
        assert_eq!(all.0[2].source, SourceId::new("C"));
    }

    #[test]
    fn test_all_object_info_merge_overwrite() {
        let src0 = ObjectCandidate {
            source: SourceId::new("A"),
            location: RemoteFileUri::new("a"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::None,
        };
        let src1 = ObjectCandidate {
            source: SourceId::new("A"),
            location: RemoteFileUri::new("a"),
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
        all.merge(&other);
        assert_eq!(all.0[0].unwind, ObjectUseInfo::Malformed);
        assert_eq!(all.0[0].debug, ObjectUseInfo::Ok);
    }

    #[test]
    fn test_all_object_info_merge_no_overwrite() {
        let src0 = ObjectCandidate {
            source: SourceId::new("A"),
            location: RemoteFileUri::new("uri://dummy"),
            download: ObjectDownloadInfo::Ok {
                features: Default::default(),
            },
            unwind: ObjectUseInfo::Ok,
            debug: ObjectUseInfo::Ok,
        };
        let src1 = ObjectCandidate {
            source: SourceId::new("A"),
            location: RemoteFileUri::new("uri://dummy"),
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
        all.merge(&other);
        assert_eq!(all.0[0].unwind, ObjectUseInfo::Ok);
        assert_eq!(all.0[0].debug, ObjectUseInfo::Ok);
    }
}
