//! Implementations for the types describing DIF object files.

use super::{
    AllObjectCandidates, Arch, CodeId, CompleteObjectInfo, DebugId, ObjectCandidate,
    ObjectFeatures, ObjectFileStatus, ObjectUseInfo, RawObjectInfo,
};
use crate::sources::{SourceId, SourceLocation};

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
    pub fn set_debug(&mut self, source: SourceId, location: SourceLocation, info: ObjectUseInfo) {
        match self
            .0
            .binary_search_by_key(&(source, location), |candidate| {
                (candidate.source.clone(), candidate.location.clone())
            }) {
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

    /// Returns `true` if the collections contains no [`ObjectCandidate`]s.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Removes all DIF object from this candidates collection.
    pub fn clear(&mut self) {
        self.0.clear()
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
