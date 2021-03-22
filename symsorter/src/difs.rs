//! Generic handling of Debug Information Files.

use std::path::PathBuf;

use symbolic::common::DebugId;

/// The type of a Debug Information File.
///
/// These are all the DIF types supported by the unified symbol server layout.
pub enum DifType {
    // Executable,
    // DebugInfo,
    // Breakpad,
    // SourceBundle,
    PList,
    BCSymbolMap,
}

pub fn get_pathname(unified_id: &DebugId, dif_type: DifType) -> PathBuf {
    let suffix = match dif_type {
        // DifType::Executable => "executable",
        // DifType::DebugInfo => "debuginfo",
        // DifType::Breakpad => "breakpad",
        // DifType::SourceBundle => "sourcebundle",
        DifType::PList => "plist",
        DifType::BCSymbolMap => "bcsymbolmap",
    };
    let repr = format!("{:x}", unified_id.uuid());
    let path = format!("{}/{}/{}", &repr[..2], &repr[2..], suffix);
    PathBuf::from(path)
}
