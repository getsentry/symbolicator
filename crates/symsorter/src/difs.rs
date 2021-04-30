//! Generic handling of Debug Information Files.

use std::fmt;
use std::path::PathBuf;

use symbolic::common::DebugId;
use symbolic::debuginfo::{FileFormat, Object, ObjectKind};

/// Gets the unified ID from an object.
pub fn get_unified_id(obj: &Object) -> String {
    if obj.file_format() == FileFormat::Pe || obj.code_id().is_none() {
        obj.debug_id().breakpad().to_string().to_lowercase()
    } else {
        // TODO: This is wrong for Windows source bundles
        obj.code_id().as_ref().unwrap().as_str().to_string()
    }
}

fn get_object_suffix(obj: &Object) -> Option<&'static str> {
    match obj.kind() {
        ObjectKind::Debug => Some("debuginfo"),
        ObjectKind::Sources if obj.file_format() == FileFormat::SourceBundle => {
            Some("sourcebundle")
        }
        ObjectKind::Relocatable | ObjectKind::Library | ObjectKind::Executable => {
            Some("executable")
        }
        _ => None,
    }
}

fn join_unified_path(id: &str, suffix: impl fmt::Display) -> PathBuf {
    format!("{}/{}/{}", &id[..2], &id[2..], suffix).into()
}

/// Returns the unified pathname for an object.
///
/// This is a relative pathname to the root of the unified symbol server layout.
pub fn get_object_path(obj: &Object) -> Option<PathBuf> {
    let id = get_unified_id(obj);
    let suffix = get_object_suffix(obj)?;
    Some(join_unified_path(&id, suffix))
}

#[derive(Clone, Copy, Debug)]
pub enum DSymAuxType {
    PList,
    BcSymbolMap,
}

impl fmt::Display for DSymAuxType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DSymAuxType::PList => write!(f, "plist"),
            DSymAuxType::BcSymbolMap => write!(f, "bcsymbolmap"),
        }
    }
}

/// Returns the unified pathname for an auxiliary file to a dSYM.
///
/// This is a relative pathname to the root of the unified symbol server layout.
pub fn get_dsym_aux_path(ty: DSymAuxType, id: DebugId) -> PathBuf {
    join_unified_path(&id.to_string(), ty)
}
