//! Generic handling of Debug Information Files.

use std::fmt::{self, Display};
use std::path::PathBuf;

use symbolic::common::DebugId;
use symbolic::debuginfo::{FileFormat, Object, ObjectKind};

/// The type of a Debug Information File.
///
/// These are all the DIF types supported by the unified symbol server layout.
#[derive(Debug, Clone, Copy)]
pub enum DifType {
    Executable,
    DebugInfo,
    // Breakpad,
    SourceBundle,
    PList,
    BCSymbolMap,
}

impl DifType {
    /// Derives the [`DifType`] for an [`Object`].
    pub fn from_object(object: &Object) -> Option<Self> {
        match object.kind() {
            ObjectKind::Debug => Some(Self::DebugInfo),
            ObjectKind::Sources => {
                if object.file_format() == FileFormat::SourceBundle {
                    Some(Self::SourceBundle)
                } else {
                    None
                }
            }
            ObjectKind::Relocatable | ObjectKind::Library | ObjectKind::Executable => {
                Some(Self::Executable)
            }
            _ => None,
        }
    }

    /// The filename suffix for this type of DIF file.
    ///
    /// If this is an extension it will **not** include the dot.
    pub fn suffix(&self) -> &'static str {
        match self {
            DifType::Executable => "executable",
            DifType::DebugInfo => "debuginfo",
            // DifType::Breakpad => "breakpad",
            DifType::SourceBundle => "sourcebundle",
            DifType::PList => "plist",
            DifType::BCSymbolMap => "bcsymbolmap",
        }
    }

    /// Returns the unified pathname for this DIF type and `unified_id`.
    ///
    /// This is a relative pathname to the root of the unified symbol server layout.
    pub fn unified_path_for(&self, unified_id: &DebugId) -> PathBuf {
        let repr = format!("{:x}", unified_id.uuid());
        let path = format!("{}/{}/{}", &repr[..2], &repr[2..], self.suffix());
        PathBuf::from(path)
    }
}

impl Display for DifType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DifType::Executable => write!(f, "Executable"),
            DifType::DebugInfo => write!(f, "DebugInfo"),
            // DifType::Breakpad => write!(f, "Breakpad"),
            DifType::SourceBundle => write!(f, "SourceBundle"),
            DifType::PList => write!(f, "PList"),
            DifType::BCSymbolMap => write!(f, "BCSymbolMap"),
        }
    }
}
