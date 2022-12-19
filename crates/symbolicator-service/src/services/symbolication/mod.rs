use symbolicator_sources::ObjectId;

use crate::services::cficaches::CfiCacheActor;
use crate::services::objects::ObjectsActor;
use crate::services::ppdb_caches::PortablePdbCacheActor;
use crate::services::symcaches::SymCacheActor;
use crate::types::RawObjectInfo;

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
