use crate::types::{DirectoryLayout, FileType, ObjectId};

pub fn get_directory_path(
    directory_layout: DirectoryLayout,
    filetype: FileType,
    identifier: &ObjectId,
) -> Option<String> {
    use DirectoryLayout::*;
    use FileType::*;

    match (directory_layout, filetype) {
        (_, PDB) => {
            // PDB (Microsoft Symbol Server)
            let debug_name = identifier.debug_name.as_ref()?;
            let debug_id = identifier.debug_id.as_ref()?;
            // XXX: Calling `breakpad` here is kinda wrong. We really only want to have no hyphens.
            Some(format!(
                "{}/{}/{}",
                debug_name,
                debug_id.breakpad(),
                debug_name
            ))
        }
        (_, PE) => {
            // PE (Microsoft Symbol Server)
            let code_name = identifier.code_name.as_ref()?;
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("{}/{}/{}", code_name, code_id, code_name))
        }
        (Symstore, ELFDebug) => {
            // ELF debug files (Microsoft Symbol Server)
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("_.debug/elf-buildid-sym-{}/_.debug", code_id))
        }
        (Native, ELFDebug) => {
            // ELF debug files (GDB format = "native")
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("{}.debug", chunk_gdb(code_id.as_str())?))
        }
        (Symstore, ELFCode) => {
            // ELF code files (Microsoft Symbol Server)
            let code_id = identifier.code_id.as_ref()?;
            let code_name = identifier.code_name.as_ref()?;
            Some(format!(
                "{}/elf-buildid-{}/{}",
                code_name, code_id, code_name
            ))
        }
        (Native, ELFCode) => {
            // ELF code files (GDB format = "native")
            let code_id = identifier.code_id.as_ref()?;
            chunk_gdb(code_id.as_str())
        }
        (Symstore, MachDebug) => {
            // Mach debug files (Microsoft Symbol Server)
            let debug_id = identifier.debug_id.as_ref()?.uuid();
            Some(format!(
                "_.dwarf/mach-uuid-sym-{}/_.dwarf",
                debug_id.to_simple_ref()
            ))
        }
        (Native, MachDebug) => {
            // Mach debug files (LLDB format = "native")
            let code_id = identifier.code_id.as_ref()?;
            chunk_lldb(code_id.as_str())
        }
        (Symstore, MachCode) => {
            // Mach code files (Microsoft Symbol Server)
            let code_name = identifier.code_name.as_ref()?;
            let debug_id = identifier.debug_id.as_ref()?.uuid();
            Some(format!(
                "{}/mach-uuid-{}/{}",
                code_name,
                debug_id.to_simple_ref(),
                code_name
            ))
        }
        (Native, MachCode) => {
            // Mach code files (LLDB format = "native")
            let code_id = identifier.code_id.as_ref()?;
            Some(format!("{}.app", chunk_lldb(code_id.as_str())?))
        }
        (_, Breakpad) => {
            // Breakpad
            let debug_name = identifier.debug_name.as_ref()?;
            let debug_id = identifier.debug_id.as_ref()?;

            let new_debug_name = if debug_name.ends_with(".exe")
                || debug_name.ends_with(".dll")
                || debug_name.ends_with(".pdb")
            {
                &debug_name[..debug_name.len() - 4]
            } else {
                &debug_name[..]
            };

            Some(format!(
                "{}.sym/{}/{}",
                new_debug_name,
                debug_id.breakpad(),
                debug_name
            ))
        }
    }
}

fn chunk_gdb(code_id: &str) -> Option<String> {
    // this is just a panic guard. It is not meant to validate the GNU build id
    if code_id.len() > 2 {
        Some(format!("{}/{}", &code_id[..2], &code_id[2..]))
    } else {
        None
    }
}

fn chunk_lldb(code_id: &str) -> Option<String> {
    // this is just a panic guard. It is not meant to validate the UUID
    if code_id.len() > 20 {
        Some(format!(
            "{}/{}/{}/{}/{}/{}",
            &code_id[0..4],
            &code_id[4..8],
            &code_id[8..12],
            &code_id[12..16],
            &code_id[16..20],
            &code_id[20..]
        ))
    } else {
        None
    }
}
