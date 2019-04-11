use std::fmt::Write;

use symbolic::common::Uuid;

use crate::types::{DirectoryLayout, FileType, FilenameCasing, ObjectId};

fn get_gdb_path(identifier: &ObjectId) -> Option<String> {
    let code_id = identifier.code_id.as_ref()?;
    let code_id = code_id.as_slice();
    if code_id.is_empty() {
        // this is just a panic guard. It is not meant to validate the GNU build id
        return None;
    }

    let mut path = String::with_capacity(code_id.len() * 2 + 1);
    write!(path, "{:02x}/", code_id[0]).ok()?;
    for byte in &code_id[1..] {
        write!(path, "{:02x}", byte).ok()?;
    }

    Some(path)
}

fn get_mach_uuid(identifier: &ObjectId) -> Option<Uuid> {
    if let Some(ref code_id) = identifier.code_id {
        Uuid::from_slice(code_id.as_slice()).ok()
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

fn get_pdb_symstore_path(identifier: &ObjectId) -> Option<String> {
    let debug_file = identifier.debug_file.as_ref()?;
    let debug_id = identifier.debug_id.as_ref()?;

    // XXX: Calling `breakpad` here is kinda wrong. We really only want to have no hyphens.
    Some(format!(
        "{}/{}/{}",
        debug_file,
        debug_id.breakpad(),
        debug_file
    ))
}

fn get_pe_symstore_path(identifier: &ObjectId) -> Option<String> {
    let code_file = identifier.code_file.as_ref()?;
    let code_id = identifier.code_id.as_ref()?;

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
        FileType::Pdb => get_pdb_symstore_path(identifier),
        FileType::Pe => get_pe_symstore_path(identifier),

        // Breakpad has its own layout similar to Microsoft Symbol Server
        // See: https://github.com/google/breakpad/blob/79ba6a494fb2097b39f76fe6a4b4b4f407e32a02/src/processor/simple_symbol_supplier.cc
        FileType::Breakpad => {
            let debug_file = identifier.debug_file.as_ref()?;
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
                "{}.sym/{}/{}",
                new_debug_file,
                debug_id.breakpad(),
                debug_file
            ))
        }
    }
}

fn get_symstore_path(filetype: FileType, identifier: &ObjectId) -> Option<String> {
    match filetype {
        FileType::ElfCode => {
            let code_id = identifier.code_id.as_ref()?;
            let code_file = identifier.code_file.as_ref()?;
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
            let code_file = identifier.code_file.as_ref()?;
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

        FileType::Pdb => get_pdb_symstore_path(identifier),
        FileType::Pe => get_pe_symstore_path(identifier),

        // Microsoft SymbolServer does not specify Breakpad.
        FileType::Breakpad => None,
    }
}

/// Determines the path for an object file in the given layout.
pub fn get_directory_path(
    directory_layout: DirectoryLayout,
    filetype: FileType,
    casing: FilenameCasing,
    identifier: &ObjectId,
) -> Option<String> {
    let mut path = match directory_layout {
        DirectoryLayout::Native => get_native_path(filetype, identifier)?,
        DirectoryLayout::Symstore => get_symstore_path(filetype, identifier)?,
    };

    match casing {
        FilenameCasing::Lowercase => path.make_ascii_lowercase(),
        FilenameCasing::Uppercase => path.make_ascii_uppercase(),
        FilenameCasing::Default => (),
    };

    Some(path)
}
