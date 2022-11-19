use symbolicator_sources::ObjectId;

use crate::services::cficaches::{CfiCacheActor, CfiCacheError};
use crate::services::objects::ObjectsActor;
use crate::services::ppdb_caches::{PortablePdbCacheActor, PortablePdbCacheError};
use crate::services::symcaches::{SymCacheActor, SymCacheError};
use crate::types::{ObjectFileStatus, RawObjectInfo};

mod apple;
mod module_lookup;
mod process_minidump;
// we should really rename this here to the `SymbolicatorService`, as it does a lot more
// than just symbolication ;-)
#[allow(clippy::module_inception)]
mod symbolication;

pub use symbolication::{StacktraceOrigin, SymbolicateStacktraces};

#[derive(Clone, Debug)]
pub struct SymbolicationActor {
    objects: ObjectsActor,
    symcaches: SymCacheActor,
    cficaches: CfiCacheActor,
    ppdb_caches: PortablePdbCacheActor,
    diagnostics_cache: crate::cache::Cache,
}

impl SymbolicationActor {
    pub fn new(
        objects: ObjectsActor,
        symcaches: SymCacheActor,
        cficaches: CfiCacheActor,
        ppdb_caches: PortablePdbCacheActor,
        diagnostics_cache: crate::cache::Cache,
    ) -> Self {
        SymbolicationActor {
            objects,
            symcaches,
            cficaches,
            ppdb_caches,
            diagnostics_cache,
        }
    }
}

impl From<&CfiCacheError> for ObjectFileStatus {
    fn from(e: &CfiCacheError) -> ObjectFileStatus {
        match e {
            CfiCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            CfiCacheError::Timeout => ObjectFileStatus::Timeout,
            CfiCacheError::ObjectParsing(_) => ObjectFileStatus::Malformed,

            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

impl From<&SymCacheError> for ObjectFileStatus {
    fn from(e: &SymCacheError) -> ObjectFileStatus {
        match e {
            SymCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            SymCacheError::Timeout => ObjectFileStatus::Timeout,
            SymCacheError::Malformed => ObjectFileStatus::Malformed,
            SymCacheError::ObjectParsing(_) => ObjectFileStatus::Malformed,
            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

impl From<&PortablePdbCacheError> for ObjectFileStatus {
    fn from(e: &PortablePdbCacheError) -> ObjectFileStatus {
        match e {
            PortablePdbCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            PortablePdbCacheError::Timeout => ObjectFileStatus::Timeout,
            PortablePdbCacheError::Malformed => ObjectFileStatus::Malformed,
            PortablePdbCacheError::PortablePdbParsing(_) => ObjectFileStatus::Malformed,
            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // PortablePdbCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

fn object_id_from_object_info(object_info: &RawObjectInfo) -> ObjectId {
    ObjectId {
        debug_id: match object_info.debug_id.as_deref() {
            None | Some("") => None,
            Some(string) => string.parse().ok(),
        },
        code_id: match object_info.code_id.as_deref() {
            None | Some("") => None,
            Some(string) => string.parse().ok(),
        },
        debug_file: object_info.debug_file.clone(),
        code_file: object_info.code_file.clone(),
        object_type: object_info.ty,
    }
}
