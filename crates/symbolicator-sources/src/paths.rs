use std::borrow::Cow;
use std::fmt::Write;

use symbolic::common::{CodeId, DebugId, Uuid};

use crate::filetype::FileType;
use crate::sources::{DirectoryLayout, DirectoryLayoutType, FilenameCasing};
use crate::types::{Glob, ObjectId, ObjectType};

const GLOB_OPTIONS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: false,
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

fn get_gdb_path(identifier: &ObjectId) -> Option<String> {
    let code_id = identifier.code_id.as_ref()?.as_str();
    if code_id.len() < 2 {
        // this is just a panic guard. It is not meant to validate the GNU build id
        return None;
    }

    Some(format!("{}/{}", &code_id[..2], &code_id[2..]))
}

fn get_mach_uuid(identifier: &ObjectId) -> Option<Uuid> {
    match identifier.code_id {
        Some(ref code_id) => code_id.as_str().parse().ok(),
        None => identifier.debug_id.as_ref().map(|debug_id| debug_id.uuid()),
    }
}

fn get_lldb_path(identifier: &ObjectId) -> Option<String> {
    let uuid = get_mach_uuid(identifier)?;
    let slice = uuid.as_bytes();

    // Format the UUID as "xxxx/xxxx/xxxx/xxxx/xxxx/xxxxxxxxxxxx"
    let mut path = String::with_capacity(37);
    for (i, byte) in slice.iter().enumerate() {
        write!(path, "{byte:02X}").ok()?;
        if i % 2 == 1 && i <= 9 {
            path.push('/');
        }
    }

    Some(path)
}

fn get_pdb_symstore_path(identifier: &ObjectId, ssqp_casing: bool) -> Option<String> {
    let debug_file = identifier.validated_debug_file_basename()?;
    let debug_id = identifier.debug_id.as_ref()?;

    let debug_file = if ssqp_casing {
        Cow::Owned(debug_file.to_lowercase())
    } else {
        Cow::Borrowed(debug_file)
    };

    let appendix = if identifier.object_type == ObjectType::PeDotnet {
        // The `Age` part of Portable PDB files is being replaced by `u32::MAX` as per:
        // https://github.com/dotnet/symstore/blob/main/docs/specs/SSQP_Key_Conventions.md#portable-pdb-signature
        u32::MAX
    } else {
        debug_id.appendix()
    };

    let debug_id = if ssqp_casing {
        format!("{:x}{:X}", debug_id.uuid().as_simple(), appendix)
    } else {
        format!("{:X}{:x}", debug_id.uuid().as_simple(), appendix)
    };

    Some(format!("{debug_file}/{debug_id}/{debug_file}"))
}

fn get_pe_symstore_path(identifier: &ObjectId, ssqp_casing: bool) -> Option<String> {
    let code_file = identifier.validated_code_file_basename()?;
    let code_id = identifier.code_id.as_ref()?.as_str();

    let code_file = if ssqp_casing {
        Cow::Owned(code_file.to_lowercase())
    } else {
        Cow::Borrowed(code_file)
    };
    let code_id = if ssqp_casing {
        Cow::Borrowed(code_id)
    } else {
        let timestamp = code_id.get(..8)?;
        let size_of_image = code_id.get(8..)?;
        Cow::Owned(format!(
            "{}{}",
            timestamp.to_uppercase(),
            size_of_image.to_lowercase()
        ))
    };

    Some(format!("{code_file}/{code_id}/{code_file}"))
}

fn get_breakpad_path(identifier: &ObjectId) -> Option<String> {
    // wasm files never get a breakpad path
    if identifier.object_type == ObjectType::Wasm {
        return None;
    }

    let debug_file = identifier.validated_debug_file_basename()?;
    let debug_id = identifier.debug_id.as_ref()?;
    let new_debug_file = debug_file
        .strip_suffix(".exe")
        .unwrap_or(debug_file)
        .strip_suffix(".dll")
        .unwrap_or(debug_file)
        .strip_suffix(".pdb")
        .unwrap_or(debug_file);

    Some(format!(
        "{}/{}/{}.sym",
        debug_file,
        debug_id.breakpad(),
        new_debug_file
    ))
}

/// Returns the relative locations on a native symbols server for the requested DIF.
///
/// Some filetypes can not be stored on a native symbol server so return an empty vector.
fn get_native_paths(filetype: FileType, identifier: &ObjectId) -> Vec<String> {
    match filetype {
        // ELF follows GDB "Build ID Method" conventions.
        // See: https://sourceware.org/gdb/onlinedocs/gdb/Separate-Debug-Files.html
        // We apply the same rule for WASM.
        FileType::ElfCode | FileType::WasmCode => get_gdb_path(identifier).into_iter().collect(),
        FileType::ElfDebug | FileType::WasmDebug => {
            if let Some(mut path) = get_gdb_path(identifier) {
                path.push_str(".debug");
                vec![path]
            } else {
                vec![]
            }
        }

        // MachO follows LLDB "File Mapped UUID Directories" conventions
        // See: http://lldb.llvm.org/symbols.html
        FileType::MachCode => {
            if let Some(mut path) = get_lldb_path(identifier) {
                path.push_str(".app");
                vec![path]
            } else {
                vec![]
            }
        }
        FileType::MachDebug => get_lldb_path(identifier).into_iter().collect(),

        // PDB and PE follows the "Symbol Server" protocol
        // See: https://docs.microsoft.com/en-us/windows/desktop/debug/using-symsrv
        FileType::Pdb => get_pdb_symstore_path(identifier, false)
            .into_iter()
            .collect(),
        FileType::Pe => get_pe_symstore_path(identifier, false)
            .into_iter()
            .collect(),
        FileType::PortablePdb => get_pdb_symstore_path(identifier, false)
            .into_iter()
            .collect(),

        // Breakpad has its own layout similar to Microsoft Symbol Server
        // See: https://github.com/google/breakpad/blob/79ba6a494fb2097b39f76fe6a4b4b4f407e32a02/src/processor/simple_symbol_supplier.cc
        FileType::Breakpad => get_breakpad_path(identifier).into_iter().collect(),

        FileType::SourceBundle => {
            let mut primary_path = match identifier.object_type {
                ObjectType::Pe | ObjectType::PeDotnet => {
                    if let Some(mut base_path) = get_pdb_symstore_path(identifier, false) {
                        if let Some(cutoff) = base_path.rfind('.') {
                            base_path.truncate(cutoff);
                        }
                        base_path
                    } else {
                        return vec![];
                    }
                }
                ObjectType::Macho => match get_lldb_path(identifier) {
                    Some(path) => path,
                    None => return vec![],
                },
                ObjectType::Elf | ObjectType::Wasm => match get_gdb_path(identifier) {
                    Some(path) => path,
                    None => return vec![],
                },
                ObjectType::Unknown => return vec![],
            };

            primary_path.push_str(".src.zip");
            let mut rv = vec![];
            if let Some(mut breakpad_path) = get_breakpad_path(identifier) {
                breakpad_path.truncate(breakpad_path.len() - 4);
                breakpad_path.push_str(".src.zip");
                if breakpad_path != primary_path {
                    rv.push(breakpad_path);
                }
            }
            rv.push(primary_path);
            rv
        }

        FileType::UuidMap => Vec::new(),
        FileType::BcSymbolMap => Vec::new(),
        FileType::Il2cpp => Vec::new(),
        FileType::Proguard => Vec::new(),
    }
}

/// Returns the relative location of the requested DIF on the SymStore symbol server.
///
/// SymStore is the public symbol server provided by Microsoft hosting debugging symbols for
/// the Windows platform.
///
/// Some file types are not supported by this symbol server and will return no result.
fn get_symstore_path(
    filetype: FileType,
    identifier: &ObjectId,
    ssqp_casing: bool,
) -> Option<String> {
    match filetype {
        FileType::ElfCode => {
            let code_id = identifier.code_id.as_ref()?;
            let code_file = identifier.validated_code_file_basename()?;
            let code_file = if ssqp_casing {
                Cow::Owned(code_file.to_lowercase())
            } else {
                Cow::Borrowed(code_file)
            };
            Some(format!("{code_file}/elf-buildid-{code_id}/{code_file}"))
        }
        FileType::ElfDebug => {
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("_.debug/elf-buildid-sym-{code_id}/_.debug"))
        }

        FileType::MachCode => {
            let code_file = identifier.validated_code_file_basename()?;
            let code_file = if ssqp_casing {
                Cow::Owned(code_file.to_lowercase())
            } else {
                Cow::Borrowed(code_file)
            };
            let uuid = get_mach_uuid(identifier)?;
            Some(format!(
                "{}/mach-uuid-{}/{}",
                code_file,
                uuid.as_simple(),
                code_file
            ))
        }
        FileType::MachDebug => {
            let uuid = get_mach_uuid(identifier)?;
            Some(format!(
                "_.dwarf/mach-uuid-sym-{}/_.dwarf",
                uuid.as_simple()
            ))
        }

        FileType::Pdb => get_pdb_symstore_path(identifier, ssqp_casing),
        FileType::PortablePdb => get_pdb_symstore_path(identifier, ssqp_casing),
        FileType::Pe => get_pe_symstore_path(identifier, ssqp_casing),

        // source bundles are available through an extension for PE/PDB only.
        FileType::SourceBundle => {
            let original_file_type = match identifier.object_type {
                ObjectType::Pe => FileType::Pdb,
                _ => return None,
            };
            let mut base_path = get_symstore_path(original_file_type, identifier, ssqp_casing)?;
            if let Some(cutoff) = base_path.rfind('.') {
                base_path.truncate(cutoff);
            }
            base_path.push_str(".src.zip");
            Some(base_path)
        }

        // Microsoft SymbolServer does not specify the following file types:
        FileType::Breakpad => None,
        FileType::WasmDebug | FileType::WasmCode => None,
        FileType::UuidMap => None,
        FileType::BcSymbolMap => None,
        FileType::Il2cpp => None,
        FileType::Proguard => None,
    }
}

fn get_symstore_index2_path(filetype: FileType, identifier: &ObjectId) -> Option<String> {
    let rv = get_symstore_path(filetype, identifier, false)?;
    if let Some(prefix) = rv.get(..2) {
        if prefix.ends_with('/') || prefix.ends_with('.') {
            return Some(format!("{}/{}", &prefix[..1], rv));
        } else {
            return Some(format!("{prefix}/{rv}"));
        }
    }
    Some(rv)
}

/// Returns the relative location of the requested DIF on the debuginfod symbol server.
///
/// Some file types are not supported by this symbol server and will return no result.
fn get_debuginfod_path(filetype: FileType, identifier: &ObjectId) -> Option<String> {
    match filetype {
        FileType::ElfCode => {
            let code_id = identifier.code_id.as_ref()?.as_str();
            Some(format!("{code_id}/executable"))
        }
        FileType::ElfDebug => {
            let code_id = identifier.code_id.as_ref()?.as_str();
            Some(format!("{code_id}/debuginfo"))
        }

        // Mach is not supported
        FileType::MachCode | FileType::MachDebug => None,

        // PDB and PE are not supported
        FileType::Pdb | FileType::Pe | FileType::PortablePdb => None,

        // WASM is not supported
        FileType::WasmDebug | FileType::WasmCode => None,

        // Breakpad is not supported
        FileType::Breakpad => None,

        // not available
        FileType::SourceBundle => None,
        FileType::UuidMap => None,
        FileType::BcSymbolMap => None,
        FileType::Il2cpp => None,
        FileType::Proguard => None,
    }
}

/// Constructs the ID for the unified symbol server.
///
/// We prefer to use the file type as indicator for going back to the object
/// type. If that is not possible, we use the object type that is stored on the
/// identifier which might be unreliable.
fn get_search_target_id(filetype: FileType, identifier: &ObjectId) -> Option<Cow<str>> {
    match filetype {
        // For these we fall back to the identifier's object type.
        FileType::SourceBundle | FileType::Breakpad | FileType::Il2cpp => {
            let filetype = match identifier.object_type {
                ObjectType::Elf => FileType::ElfCode,
                ObjectType::Macho => FileType::MachCode,
                ObjectType::Pe => FileType::Pe,
                ObjectType::Wasm => FileType::WasmCode,
                ObjectType::PeDotnet => FileType::PortablePdb,
                // guess we're out of luck.
                ObjectType::Unknown => return None,
            };
            get_search_target_id(filetype, identifier)
        }

        // PEs and PDBs are indexed by the debug id in lowercase breakpad format
        // always.  This is done because code IDs by themselves are not reliable
        // enough for PEs and are only useful together with the file name which
        // we do not want to encode.
        FileType::Pe | FileType::Pdb | FileType::PortablePdb => Some(Cow::Owned(
            identifier.debug_id?.breakpad().to_string().to_lowercase(),
        )),

        // On mach we can always determine the code ID from the debug ID if the
        // code ID is unavailable.  We apply the same rule to WASM files as well as
        // auxiliary DIFs, as we suggest Uuids to be used as build ids.
        FileType::MachCode
        | FileType::MachDebug
        | FileType::WasmDebug
        | FileType::WasmCode
        | FileType::UuidMap
        | FileType::BcSymbolMap => {
            if identifier.code_id.is_none() {
                Some(Cow::Owned(
                    identifier.debug_id?.uuid().as_simple().to_string(),
                ))
            } else {
                Some(Cow::Borrowed(identifier.code_id.as_ref()?.as_str()))
            }
        }
        // For ELF we always use the code ID.  If it's not available we can't actually
        // find this file at all.  See symsorter which will never use the debug ID for
        // such files.
        FileType::ElfCode | FileType::ElfDebug => {
            Some(Cow::Borrowed(identifier.code_id.as_ref()?.as_str()))
        }
        FileType::Proguard => None,
    }
}

fn get_unified_path(filetype: FileType, identifier: &ObjectId) -> Option<String> {
    // determine the suffix and object type
    let suffix = match filetype {
        FileType::ElfCode | FileType::MachCode | FileType::Pe | FileType::WasmCode => "executable",
        FileType::ElfDebug
        | FileType::MachDebug
        | FileType::Pdb
        | FileType::WasmDebug
        | FileType::PortablePdb => "debuginfo",
        FileType::Breakpad => "breakpad",
        FileType::SourceBundle => "sourcebundle",
        FileType::UuidMap => "uuidmap",
        FileType::BcSymbolMap => "bcsymbolmap",
        FileType::Il2cpp => "il2cpp",
        FileType::Proguard => "proguard",
    };

    // determine the ID we use for the path
    let id = get_search_target_id(filetype, identifier)?;

    Some(format!("{}/{}/{}", id.get(..2)?, id.get(2..)?, suffix))
}

fn get_slashsymbols_path(identifier: &ObjectId) -> Option<String> {
    let code_id = identifier.code_id.as_ref()?;
    let code_file = identifier.validated_code_file_basename()?;
    Some(format!("{code_file}/{code_id}/symbols"))
}

/// Determines the paths for an object file in the given layout.
///
/// The vector is ordered from lower priority to highest priority.
pub fn get_directory_paths(
    directory_layout: DirectoryLayout,
    filetype: FileType,
    identifier: &ObjectId,
) -> Vec<String> {
    let mut paths: Vec<String> = match directory_layout.ty {
        DirectoryLayoutType::Native => get_native_paths(filetype, identifier),
        DirectoryLayoutType::Symstore => get_symstore_path(filetype, identifier, false)
            .into_iter()
            .collect(),
        DirectoryLayoutType::SymstoreIndex2 => get_symstore_index2_path(filetype, identifier)
            .into_iter()
            .collect(),
        DirectoryLayoutType::Ssqp => get_symstore_path(filetype, identifier, true)
            .into_iter()
            .collect(),
        DirectoryLayoutType::Debuginfod => get_debuginfod_path(filetype, identifier)
            .into_iter()
            .collect(),
        DirectoryLayoutType::Unified => {
            get_unified_path(filetype, identifier).into_iter().collect()
        }
        DirectoryLayoutType::SlashSymbols => {
            get_slashsymbols_path(identifier).into_iter().collect()
        }
    };

    for path in paths.iter_mut() {
        match directory_layout.casing {
            FilenameCasing::Lowercase => path.make_ascii_lowercase(),
            FilenameCasing::Uppercase => path.make_ascii_uppercase(),
            FilenameCasing::Default => (),
        }
    }

    // when fetching PE and PDB files we generally allow the also the
    // compressed matches (last char subtituted with an underscore)
    if filetype == FileType::Pdb || filetype == FileType::Pe {
        paths = paths
            .into_iter()
            .flat_map(|path| {
                let mut compressed_path = path.clone();
                compressed_path.pop();
                compressed_path.push('_');
                Some(compressed_path).into_iter().chain(Some(path))
            })
            .collect();
    };

    paths
}

/// Parses a symstore path into a possible [`FileType`] and an [`ObjectId`].
pub fn parse_symstore_path(path: &str) -> Option<(&'static [FileType], ObjectId)> {
    let mut split = path.splitn(3, '/');
    let leading_fn = split.next()?;
    let signature = split.next()?;
    let trailing_fn = split.next()?;

    let leading_fn_lower = leading_fn.to_lowercase();
    if !leading_fn_lower.eq_ignore_ascii_case(trailing_fn) {
        return None;
    }

    let signature_lower = signature.to_lowercase();
    if leading_fn_lower.ends_with(".debug") && signature_lower.starts_with("elf-buildid-sym-") {
        Some((
            &[FileType::ElfDebug],
            ObjectId {
                code_id: Some(CodeId::new(signature[16..].into())),
                code_file: Some(leading_fn.into()),
                debug_id: None,
                debug_file: None,
                debug_checksum: None,
                object_type: ObjectType::Elf,
            },
        ))
    } else if signature_lower.starts_with("elf-buildid-") {
        Some((
            &[FileType::ElfCode],
            ObjectId {
                code_id: Some(CodeId::new(signature[12..].into())),
                code_file: Some(leading_fn.into()),
                debug_id: None,
                debug_file: None,
                debug_checksum: None,
                object_type: ObjectType::Elf,
            },
        ))
    } else if leading_fn_lower.ends_with(".dwarf") && signature_lower.starts_with("mach-uuid-sym-")
    {
        Some((
            &[FileType::MachDebug],
            ObjectId {
                code_id: Some(CodeId::new(signature[14..].into())),
                code_file: Some(leading_fn.into()),
                debug_id: None,
                debug_file: None,
                debug_checksum: None,
                object_type: ObjectType::Macho,
            },
        ))
    } else if signature_lower.starts_with("mach-uuid-") {
        Some((
            &[FileType::MachCode],
            ObjectId {
                code_id: Some(CodeId::new(signature[10..].into())),
                code_file: Some(leading_fn.into()),
                debug_id: None,
                debug_file: None,
                debug_checksum: None,
                object_type: ObjectType::Macho,
            },
        ))
    } else if leading_fn_lower.ends_with(".pdb") {
        Some((
            &[FileType::Pdb],
            ObjectId {
                code_id: None,
                code_file: None,
                debug_id: Some(DebugId::from_breakpad(signature).ok()?),
                debug_file: Some(leading_fn.into()),
                debug_checksum: None,
                object_type: ObjectType::Pe,
            },
        ))
    } else {
        Some((
            &[FileType::Pe],
            ObjectId {
                code_id: Some(CodeId::new(signature.to_owned())),
                code_file: Some(leading_fn.into()),
                debug_id: None,
                debug_file: None,
                debug_checksum: None,
                object_type: ObjectType::Pe,
            },
        ))
    }
}

/// Checks whether an [`ObjectId`] matches any of the [`Glob`] patterns.
pub fn matches_path_patterns(object_id: &ObjectId, patterns: &[Glob]) -> bool {
    fn canonicalize_path(s: &str) -> String {
        s.replace('\\', "/")
    }

    if patterns.is_empty() {
        return true;
    }

    for pattern in patterns {
        if let Some(ref path) = object_id.code_file {
            if pattern.matches_with(&canonicalize_path(path), GLOB_OPTIONS) {
                return true;
            }
        }

        if let Some(ref path) = object_id.debug_file {
            if pattern.matches_with(&canonicalize_path(path), GLOB_OPTIONS) {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::LazyLock;

    static PE_OBJECT_ID: LazyLock<ObjectId> = LazyLock::new(|| ObjectId {
        code_id: Some("5ab380779000".parse().unwrap()),
        code_file: Some("C:\\projects\\breakpad-tools\\windows\\Release\\crash.exe".into()),
        debug_id: Some("3249d99d-0c40-4931-8610-f4e4fb0b6936-1".parse().unwrap()),
        debug_file: Some("C:\\projects\\breakpad-tools\\windows\\Release\\crash.pdb".into()),
        debug_checksum: None,
        object_type: ObjectType::Pe,
    });
    static MACHO_OBJECT_ID: LazyLock<ObjectId> = LazyLock::new(|| ObjectId {
        code_id: None,
        code_file: Some("/Users/travis/build/getsentry/breakpad-tools/macos/build/./crash".into()),
        debug_id: Some("67e9247c-814e-392b-a027-dbde6748fcbf".parse().unwrap()),
        debug_file: Some("crash".into()),
        debug_checksum: None,
        object_type: ObjectType::Macho,
    });
    static ELF_OBJECT_ID: LazyLock<ObjectId> = LazyLock::new(|| ObjectId {
        code_id: Some("dfb85de42daffd09640c8fe377d572de3e168920".parse().unwrap()),
        code_file: Some("/lib/x86_64-linux-gnu/libm-2.23.so".into()),
        debug_id: Some("e45db8df-af2d-09fd-640c-8fe377d572de".parse().unwrap()),
        debug_file: Some("/lib/x86_64-linux-gnu/libm-2.23.so".into()),
        debug_checksum: None,
        object_type: ObjectType::Elf,
    });
    static WASM_OBJECT_ID: LazyLock<ObjectId> = LazyLock::new(|| ObjectId {
        code_id: Some("67e9247c814e392ba027dbde6748fcbf".parse().unwrap()),
        code_file: None,
        debug_id: Some("67e9247c-814e-392b-a027-dbde6748fcbf".parse().unwrap()),
        debug_file: Some("file://foo.invalid/demo.wasm".into()),
        debug_checksum: None,
        object_type: ObjectType::Wasm,
    });

    fn pattern(x: &str) -> Glob {
        Glob(x.parse().unwrap())
    }

    #[test]
    fn test_get_native_path() {
        macro_rules! path_test {
            ($filetype:expr, $obj:expr, @$output:literal) => {
                insta::assert_snapshot!(get_native_paths($filetype, &$obj).join("\n"), @$output);
            };
        }

        path_test!(FileType::Pdb, PE_OBJECT_ID, @"crash.pdb/3249D99D0C4049318610F4E4FB0B69361/crash.pdb");
        path_test!(FileType::Pe, PE_OBJECT_ID, @"crash.exe/5AB380779000/crash.exe");
        path_test!(FileType::Breakpad, PE_OBJECT_ID, @"crash.pdb/3249D99D0C4049318610F4E4FB0B69361/crash.sym");
        path_test!(FileType::SourceBundle, PE_OBJECT_ID, @"crash.pdb/3249D99D0C4049318610F4E4FB0B69361/crash.src.zip");
        path_test!(FileType::MachCode, MACHO_OBJECT_ID, @"67E9/247C/814E/392B/A027/DBDE6748FCBF.app");
        path_test!(FileType::MachDebug, MACHO_OBJECT_ID, @"67E9/247C/814E/392B/A027/DBDE6748FCBF");
        path_test!(FileType::Breakpad, MACHO_OBJECT_ID, @"crash/67E9247C814E392BA027DBDE6748FCBF0/crash.sym");
        path_test!(FileType::SourceBundle, MACHO_OBJECT_ID, @r###"
        crash/67E9247C814E392BA027DBDE6748FCBF0/crash.src.zip
        67E9/247C/814E/392B/A027/DBDE6748FCBF.src.zip
        "###);
        path_test!(FileType::WasmDebug, WASM_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf.debug");
        path_test!(FileType::WasmCode, WASM_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf");
        path_test!(FileType::SourceBundle, WASM_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf.src.zip");
        path_test!(FileType::ElfCode, ELF_OBJECT_ID, @"df/b85de42daffd09640c8fe377d572de3e168920");
        path_test!(FileType::ElfDebug, ELF_OBJECT_ID, @"df/b85de42daffd09640c8fe377d572de3e168920.debug");
        path_test!(FileType::Breakpad, ELF_OBJECT_ID, @"libm-2.23.so/E45DB8DFAF2D09FD640C8FE377D572DE0/libm-2.23.so.sym");
        path_test!(FileType::SourceBundle, ELF_OBJECT_ID, @r###"
        libm-2.23.so/E45DB8DFAF2D09FD640C8FE377D572DE0/libm-2.23.so.src.zip
        df/b85de42daffd09640c8fe377d572de3e168920.src.zip
        "###);
    }

    #[test]
    fn test_get_unified_path() {
        macro_rules! path_test {
            ($filetype:expr, $obj:expr, @$output:literal) => {
                insta::assert_snapshot!(get_unified_path($filetype, &$obj).unwrap(), @$output);
            };
        }

        path_test!(FileType::Pdb, PE_OBJECT_ID, @"32/49d99d0c4049318610f4e4fb0b69361/debuginfo");
        path_test!(FileType::Pe, PE_OBJECT_ID, @"32/49d99d0c4049318610f4e4fb0b69361/executable");
        path_test!(FileType::Breakpad, PE_OBJECT_ID, @"32/49d99d0c4049318610f4e4fb0b69361/breakpad");
        path_test!(FileType::SourceBundle, PE_OBJECT_ID, @"32/49d99d0c4049318610f4e4fb0b69361/sourcebundle");
        path_test!(FileType::MachCode, MACHO_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf/executable");
        path_test!(FileType::MachDebug, MACHO_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf/debuginfo");
        path_test!(FileType::WasmDebug, WASM_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf/debuginfo");
        path_test!(FileType::WasmCode, WASM_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf/executable");
        path_test!(FileType::Breakpad, MACHO_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf/breakpad");
        path_test!(FileType::SourceBundle, MACHO_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf/sourcebundle");
        path_test!(FileType::ElfCode, ELF_OBJECT_ID, @"df/b85de42daffd09640c8fe377d572de3e168920/executable");
        path_test!(FileType::ElfDebug, ELF_OBJECT_ID, @"df/b85de42daffd09640c8fe377d572de3e168920/debuginfo");
        path_test!(FileType::Breakpad, ELF_OBJECT_ID, @"df/b85de42daffd09640c8fe377d572de3e168920/breakpad");
        path_test!(FileType::SourceBundle, ELF_OBJECT_ID, @"df/b85de42daffd09640c8fe377d572de3e168920/sourcebundle");
        path_test!(FileType::SourceBundle, WASM_OBJECT_ID, @"67/e9247c814e392ba027dbde6748fcbf/sourcebundle");
    }

    #[test]
    fn test_get_debuginfod_path() {
        macro_rules! path_test {
            ($filetype:expr, $obj:expr, @$output:literal) => {
                insta::assert_snapshot!(get_debuginfod_path($filetype, &$obj).unwrap(), @$output);
            };
        }

        path_test!(FileType::ElfCode, ELF_OBJECT_ID, @"dfb85de42daffd09640c8fe377d572de3e168920/executable");
        path_test!(FileType::ElfDebug, ELF_OBJECT_ID, @"dfb85de42daffd09640c8fe377d572de3e168920/debuginfo");
    }

    #[test]
    fn test_get_symstore_path() {
        macro_rules! path_test {
            ($filetype:expr, $obj:expr, @$output:literal) => {
                insta::assert_snapshot!(get_symstore_path($filetype, &$obj, false).unwrap(), @$output);
            };
        }

        path_test!(FileType::Pdb, PE_OBJECT_ID, @"crash.pdb/3249D99D0C4049318610F4E4FB0B69361/crash.pdb");
        path_test!(FileType::Pe, PE_OBJECT_ID, @"crash.exe/5AB380779000/crash.exe");
        path_test!(FileType::SourceBundle, PE_OBJECT_ID, @"crash.pdb/3249D99D0C4049318610F4E4FB0B69361/crash.src.zip");
        path_test!(FileType::MachCode, MACHO_OBJECT_ID, @"crash/mach-uuid-67e9247c814e392ba027dbde6748fcbf/crash");
        path_test!(FileType::MachDebug, MACHO_OBJECT_ID, @"_.dwarf/mach-uuid-sym-67e9247c814e392ba027dbde6748fcbf/_.dwarf");
        path_test!(FileType::ElfCode, ELF_OBJECT_ID, @"libm-2.23.so/elf-buildid-dfb85de42daffd09640c8fe377d572de3e168920/libm-2.23.so");
        path_test!(FileType::ElfDebug, ELF_OBJECT_ID, @"_.debug/elf-buildid-sym-dfb85de42daffd09640c8fe377d572de3e168920/_.debug");
    }

    #[test]
    fn test_get_symstore_index2_path() {
        macro_rules! path_test {
            ($filetype:expr, $obj:expr, @$output:literal) => {
                insta::assert_snapshot!(get_symstore_index2_path($filetype, &$obj).unwrap(), @$output);
            };
        }

        path_test!(FileType::Pdb, PE_OBJECT_ID, @"cr/crash.pdb/3249D99D0C4049318610F4E4FB0B69361/crash.pdb");
        path_test!(FileType::Pe, PE_OBJECT_ID, @"cr/crash.exe/5AB380779000/crash.exe");
        path_test!(FileType::SourceBundle, PE_OBJECT_ID, @"cr/crash.pdb/3249D99D0C4049318610F4E4FB0B69361/crash.src.zip");
        path_test!(FileType::MachCode, MACHO_OBJECT_ID, @"cr/crash/mach-uuid-67e9247c814e392ba027dbde6748fcbf/crash");
        path_test!(FileType::MachDebug, MACHO_OBJECT_ID, @"_/_.dwarf/mach-uuid-sym-67e9247c814e392ba027dbde6748fcbf/_.dwarf");
        path_test!(FileType::ElfCode, ELF_OBJECT_ID, @"li/libm-2.23.so/elf-buildid-dfb85de42daffd09640c8fe377d572de3e168920/libm-2.23.so");
        path_test!(FileType::ElfDebug, ELF_OBJECT_ID, @"_/_.debug/elf-buildid-sym-dfb85de42daffd09640c8fe377d572de3e168920/_.debug");
    }

    #[test]
    fn test_slash_symbols() {
        let paths = get_directory_paths(
            DirectoryLayout {
                ty: DirectoryLayoutType::SlashSymbols,
                ..Default::default()
            },
            FileType::ElfCode,
            &ELF_OBJECT_ID,
        );
        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0],
            "libm-2.23.so/dfb85de42daffd09640c8fe377d572de3e168920/symbols"
        );
    }

    #[test]
    fn test_matches_path_patterns_empty() {
        assert!(matches_path_patterns(
            &ObjectId {
                code_file: Some("C:\\Windows\\System32\\kernel32.dll".to_owned()),
                ..Default::default()
            },
            &[]
        ));
    }

    #[test]
    fn test_matches_path_patterns_single_star() {
        assert!(matches_path_patterns(
            &ObjectId {
                code_file: Some("C:\\Windows\\System32\\kernel32.dll".to_owned()),
                ..Default::default()
            },
            &[pattern("c:/windows/*")]
        ));
    }

    #[test]
    fn test_matches_path_patterns_drive_letter_wildcard() {
        assert!(matches_path_patterns(
            &ObjectId {
                code_file: Some("C:\\Windows\\System32\\kernel32.dll".to_owned()),
                ..Default::default()
            },
            &[pattern("?:/windows/*")]
        ));
    }

    #[test]
    fn test_matches_path_patterns_drive_letter() {
        assert!(!matches_path_patterns(
            &ObjectId {
                code_file: Some("C:\\Windows\\System32\\kernel32.dll".to_owned()),
                ..Default::default()
            },
            &[pattern("d:/windows/**")]
        ));
    }

    #[test]
    fn test_parsing_symstore_paths() {
        let (types, id) = parse_symstore_path("foo.exe/542D574Ec2000/foo.exe").unwrap();
        assert_eq!(types, [FileType::Pe]);
        assert_eq!(id.code_id.unwrap().as_str(), "542d574ec2000");

        let (types, id) =
            parse_symstore_path("foo.pdb/497b72f6390a44fc878e5a2d63b6cc4b1/foo.pdb").unwrap();
        assert_eq!(types, [FileType::Pdb]);
        assert_eq!(
            id.debug_id.unwrap().to_string(),
            "497b72f6-390a-44fc-878e-5a2d63b6cc4b-1"
        );

        let (types, id) =
            parse_symstore_path("foo.pdb/497b72f6390a44fc878e5a2d63b6cc4bFFFFFFFF/foo.pdb")
                .unwrap();
        assert_eq!(types, [FileType::Pdb]);
        assert_eq!(
            id.debug_id.unwrap().to_string(),
            "497b72f6-390a-44fc-878e-5a2d63b6cc4b-ffffffff"
        );

        let (types, id) = parse_symstore_path(
            "foo.so/elf-buildid-180a373d6afbabf0eb1f09be1bc45bd796a71085/foo.so",
        )
        .unwrap();
        assert_eq!(types, [FileType::ElfCode]);
        assert_eq!(
            id.code_id.unwrap().as_str(),
            "180a373d6afbabf0eb1f09be1bc45bd796a71085"
        );

        let (types, id) = parse_symstore_path(
            "_.debug/elf-buildid-sym-180a373d6afbabf0eb1f09be1bc45bd796a71085/_.debug",
        )
        .unwrap();
        assert_eq!(types, [FileType::ElfDebug]);
        assert_eq!(
            id.code_id.unwrap().as_str(),
            "180a373d6afbabf0eb1f09be1bc45bd796a71085"
        );

        let (types, id) = parse_symstore_path(
            "_.debug/elf-buildid-sym-180a373d6afbabf0eb1f09be1bc45bd700000000/_.debug",
        )
        .unwrap();
        assert_eq!(types, [FileType::ElfDebug]);
        assert_eq!(
            id.code_id.unwrap().as_str(),
            "180a373d6afbabf0eb1f09be1bc45bd700000000"
        );

        let (types, id) =
            parse_symstore_path("foo.dylib/mach-uuid-497b72f6390a44fc878e5a2d63b6cc4b/foo.dylib")
                .unwrap();
        assert_eq!(types, [FileType::MachCode]);
        assert_eq!(
            id.code_id.unwrap().as_str(),
            "497b72f6390a44fc878e5a2d63b6cc4b"
        );

        let (types, id) =
            parse_symstore_path("_.dwarf/mach-uuid-sym-497b72f6390a44fc878e5a2d63b6cc4b/_.dwarf")
                .unwrap();
        assert_eq!(types, [FileType::MachDebug]);
        assert_eq!(
            id.code_id.unwrap().as_str(),
            "497b72f6390a44fc878e5a2d63b6cc4b"
        );
    }

    /// Tests that all code/debug file values we have successfully downloaded
    /// from the Electron source ar covered by a small number of glob patterns.
    #[test]
    fn test_electron_path_patterns() {
        let patterns = [
            pattern("*electron*"),
            pattern("*ffmpeg*"),
            pattern("*libEGL*"),
            pattern("*libGLESv2*"),
            pattern("*slack*"),
            pattern("*vk_swiftshader*"),
        ];

        let files = [
            (
                "C:\\Program Files\\WindowsApps\\91750D7E.Slack_4.35.126.0_x64__8she8kybcnzg4\\app\\Slack.exe",
                "C:\\projects\\src\\out\\Default\\electron.exe.pdb",
            ),
            (
                "/Applications/Program.app/Contents/Frameworks/Electron Framework.framework/Versions/A/Electron Framework",
                "Electron Framework",
            ),
            (
                "/Users/Sebastian/github/project/node_modules/.pnpm/electron@33.0.2/node_modules/electron/dist/Electron.app/Contents/Frameworks/Electron Helper.app/Contents/MacOS/Electron Helper",
                "Electron Helper",
            ),
            (
                "C:\\Users\\Sebastian\\AppData\\Local\\Programs\\Program\\ffmpeg.dll",
                "C:\\projects\\src\\out\\Default\\ffmpeg.dll.pdb",
            ),
            (
                "/Applications/Program.app/Contents/Frameworks/Electron Framework.framework/Versions/A/Libraries/libEGL.dylib",
                "libEGL.dylib",
            ),
            (
                "C:\\Users\\******\\AppData\\Local\\AmazonChime\\app-5.23.32022\\libglesv2.dll",
                "C:\\projects\\src\\out\\Default\\libGLESv2.dll.pdb",
            ),
            (
                "/Applications/Program.app/Contents/Frameworks/Electron Framework.framework/Versions/A/Libraries/libGLESv2.dylib",
                "libGLESv2.dylib",
            ),
            ("libGLESv2.so", "libGLESv2.so"),
            (
                "/Applications/Program.app/Contents/Frameworks/Slack Helper.app/Contents/MacOS/Slack Helper",
                "Slack Helper",
            ),
            (
                "/Applications/Program.app/Contents/Frameworks/Slack Helper (GPU).app/Contents/MacOS/Slack Helper (GPU)",
                "Slack Helper (GPU)",
            ),
            (
                "/Volumes/Program/Program.app/Contents/Frameworks/Slack Helper (Renderer).app/Contents/MacOS/Slack Helper (Renderer)",
                "Slack Helper (Renderer)",
            ),
            ("/Applications/Program.app/Contents/MacOS/Slack", "Slack"),
            (
                "C:\\Users\\Sebastian\\AppData\\Local\\Programs\\Program\\vk_swiftshader.dll",
                "C:\\projects\\src\\out\\Default\\vk_swiftshader.dll.pdb",
            ),
        ];

        for (code_file, debug_file) in files {
            let object_id = ObjectId {
                code_file: Some(code_file.into()),
                debug_file: Some(debug_file.into()),
                ..Default::default()
            };

            assert!(
                matches_path_patterns(&object_id, &patterns),
                "Non-matching object_id: {object_id:?}"
            );
        }
    }
}
