use std::borrow::Cow;
use std::fmt::Write;

use symbolic::common::{CodeId, DebugId, Uuid};

use crate::types::{
    DirectoryLayout, DirectoryLayoutType, FileType, FilenameCasing, Glob, ObjectId,
};

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
    if let Some(ref code_id) = identifier.code_id {
        code_id.as_str().parse().ok()
    } else if let Some(ref debug_id) = identifier.debug_id {
        Some(debug_id.uuid())
    } else {
        None
    }
}

fn get_lldb_path(identifier: &ObjectId) -> Option<String> {
    let uuid = get_mach_uuid(identifier)?;
    let slice = uuid.as_bytes();

    // Format the UUID as "xxxx/xxxx/xxxx/xxxx/xxxx/xxxxxxxxxxxx"
    let mut path = String::with_capacity(37);
    for (i, byte) in slice.iter().enumerate() {
        write!(path, "{:02X}", byte).ok()?;
        if i % 2 == 1 && i <= 9 {
            path.push('/');
        }
    }

    Some(path)
}

fn get_pdb_symstore_path(identifier: &ObjectId, ssqp_casing: bool) -> Option<String> {
    let debug_file = identifier.debug_file_basename()?;
    let debug_id = identifier.debug_id.as_ref()?;

    let debug_file = if ssqp_casing {
        Cow::Owned(debug_file.to_lowercase())
    } else {
        Cow::Borrowed(debug_file)
    };
    let debug_id = if ssqp_casing {
        format!(
            "{:x}{:X}",
            debug_id.uuid().to_simple_ref(),
            debug_id.appendix()
        )
    } else {
        format!(
            "{:X}{:x}",
            debug_id.uuid().to_simple_ref(),
            debug_id.appendix()
        )
    };

    Some(format!("{}/{}/{}", debug_file, debug_id, debug_file))
}

fn get_pe_symstore_path(identifier: &ObjectId, ssqp_casing: bool) -> Option<String> {
    let code_file = identifier.code_file_basename()?;
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

    Some(format!("{}/{}/{}", code_file, code_id, code_file))
}

fn get_native_path(filetype: FileType, identifier: &ObjectId) -> Option<String> {
    match filetype {
        // ELF follows GDB "Build ID Method" conventions.
        // See: https://sourceware.org/gdb/onlinedocs/gdb/Separate-Debug-Files.html
        FileType::ElfCode => get_gdb_path(identifier),
        FileType::ElfDebug => {
            let mut path = get_gdb_path(identifier)?;
            path.push_str(".debug");
            Some(path)
        }

        // MachO follows LLDB "File Mapped UUID Directories" conventions
        // See: http://lldb.llvm.org/symbols.html
        FileType::MachCode => {
            let mut path = get_lldb_path(identifier)?;
            path.push_str(".app");
            Some(path)
        }
        FileType::MachDebug => get_lldb_path(identifier),

        // PDB and PE follows the "Symbol Server" protocol
        // See: https://docs.microsoft.com/en-us/windows/desktop/debug/using-symsrv
        FileType::Pdb => get_pdb_symstore_path(identifier, false),
        FileType::Pe => get_pe_symstore_path(identifier, false),

        // Breakpad has its own layout similar to Microsoft Symbol Server
        // See: https://github.com/google/breakpad/blob/79ba6a494fb2097b39f76fe6a4b4b4f407e32a02/src/processor/simple_symbol_supplier.cc
        FileType::Breakpad => {
            let debug_file = identifier.debug_file_basename()?;
            let debug_id = identifier.debug_id.as_ref()?;

            let new_debug_file = if debug_file.ends_with(".exe")
                || debug_file.ends_with(".dll")
                || debug_file.ends_with(".pdb")
            {
                &debug_file[..debug_file.len() - 4]
            } else {
                &debug_file[..]
            };

            Some(format!(
                "{}/{}/{}.sym",
                debug_file,
                debug_id.breakpad(),
                new_debug_file
            ))
        }

        // not available
        FileType::SourceBundle => None,
    }
}

fn get_symstore_path(
    filetype: FileType,
    identifier: &ObjectId,
    ssqp_casing: bool,
) -> Option<String> {
    match filetype {
        FileType::ElfCode => {
            let code_id = identifier.code_id.as_ref()?;
            let code_file = identifier.code_file_basename()?;
            let code_file = if ssqp_casing {
                Cow::Owned(code_file.to_lowercase())
            } else {
                Cow::Borrowed(code_file)
            };
            Some(format!(
                "{}/elf-buildid-{}/{}",
                code_file, code_id, code_file
            ))
        }
        FileType::ElfDebug => {
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("_.debug/elf-buildid-sym-{}/_.debug", code_id))
        }

        FileType::MachCode => {
            let code_file = identifier.code_file_basename()?;
            let code_file = if ssqp_casing {
                Cow::Owned(code_file.to_lowercase())
            } else {
                Cow::Borrowed(code_file)
            };
            let uuid = get_mach_uuid(identifier)?;
            Some(format!(
                "{}/mach-uuid-{}/{}",
                code_file,
                uuid.to_simple_ref(),
                code_file
            ))
        }
        FileType::MachDebug => {
            let uuid = get_mach_uuid(identifier)?;
            Some(format!(
                "_.dwarf/mach-uuid-sym-{}/_.dwarf",
                uuid.to_simple_ref()
            ))
        }

        FileType::Pdb => get_pdb_symstore_path(identifier, ssqp_casing),
        FileType::Pe => get_pe_symstore_path(identifier, ssqp_casing),

        // Microsoft SymbolServer does not specify Breakpad.
        FileType::Breakpad => None,

        // not available
        FileType::SourceBundle => None,
    }
}

fn get_symstore_index2_path(filetype: FileType, identifier: &ObjectId) -> Option<String> {
    let rv = get_symstore_path(filetype, identifier, false)?;
    if let Some(prefix) = rv.get(..2) {
        if prefix.ends_with('/') || prefix.ends_with('.') {
            return Some(format!("{}/{}", &prefix[..1], rv));
        } else {
            return Some(format!("{}/{}", prefix, rv));
        }
    }
    Some(rv)
}

fn get_debuginfod_path(filetype: FileType, identifier: &ObjectId) -> Option<String> {
    match filetype {
        FileType::ElfCode | FileType::MachCode => {
            let code_id = identifier.code_id.as_ref()?.as_str();
            Some(format!("{}/executable", code_id))
        }
        FileType::ElfDebug | FileType::MachDebug => {
            let code_id = identifier.code_id.as_ref()?.as_str();
            Some(format!("{}/debuginfo", code_id))
        }

        // PDB and PE are not supported
        FileType::Pdb | FileType::Pe => None,

        // Breakpad is not supported
        FileType::Breakpad => None,

        // not available
        FileType::SourceBundle => None,
    }
}

/// Determines the path for an object file in the given layout.
pub fn get_directory_path(
    directory_layout: DirectoryLayout,
    filetype: FileType,
    identifier: &ObjectId,
) -> Option<String> {
    let mut path = match directory_layout.ty {
        DirectoryLayoutType::Native => get_native_path(filetype, identifier)?,
        DirectoryLayoutType::Symstore => get_symstore_path(filetype, identifier, false)?,
        DirectoryLayoutType::SymstoreIndex2 => get_symstore_index2_path(filetype, identifier)?,
        DirectoryLayoutType::SSQP => get_symstore_path(filetype, identifier, true)?,
        DirectoryLayoutType::Debuginfod => get_debuginfod_path(filetype, identifier)?,
    };

    match directory_layout.casing {
        FilenameCasing::Lowercase => path.make_ascii_lowercase(),
        FilenameCasing::Uppercase => path.make_ascii_uppercase(),
        FilenameCasing::Default => (),
    };

    Some(path)
}

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
            },
        ))
    }
}

pub fn matches_path_patterns(object_id: &ObjectId, patterns: &[Glob]) -> bool {
    fn canonicalize_path(s: &str) -> String {
        s.replace(r"\", "/")
    }

    if patterns.is_empty() {
        return true;
    }

    for pattern in patterns {
        for path in &[&object_id.code_file, &object_id.debug_file] {
            if let Some(ref path) = path {
                if pattern.matches_with(&canonicalize_path(path), GLOB_OPTIONS) {
                    return true;
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(x: &str) -> Glob {
        Glob(x.parse().unwrap())
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
}
