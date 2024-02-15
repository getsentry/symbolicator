use serde::{Deserialize, Serialize};

use crate::types::ObjectType;

/// Different file types that can be fetched from symbol sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileType {
    /// Windows/PDB code files
    Pe,
    /// Windows/PDB debug files
    Pdb,
    /// Portable PDB files used for .NET
    #[serde(rename = "portablepdb")]
    PortablePdb,
    /// Macos/Mach debug files
    MachDebug,
    /// Macos/Mach code files
    MachCode,
    /// Linux/ELF debug files
    ElfDebug,
    /// Linux/ELF code files
    ElfCode,
    /// A WASM debug file
    WasmDebug,
    /// A WASM code file
    WasmCode,
    /// Breakpad files (this is the reason we have a flat enum for what at first sight could've
    /// been two enums)
    Breakpad,
    /// Source bundle
    #[serde(rename = "sourcebundle")]
    SourceBundle,
    /// A file mapping a MachO [`DebugId`] to an originating [`DebugId`].
    ///
    /// For the MachO format a [`DebugId`] is always a UUID.
    ///
    /// This is used when compilation introduces intermediate outputs, like Apple BitCode.
    /// In this case some Debug Information Files will have the [`DebugId`] of the
    /// intermediate compilation rather than of the final executable code.  Thus these maps
    /// point to which other [`DebugId`]s provide DIFs.
    ///
    /// At the time of writing this is only used to map a dSYM UUID to a BCSymbolMap UUID
    /// for MachO.  The only format supported for this is currently the XML PropertyList
    /// format.  In the future other formats could be added to this.
    ///
    /// [`DebugId`]: symbolic::common::DebugId
    #[serde(rename = "uuidmap")]
    UuidMap,
    /// BCSymbolMap, de-obfuscates symbol names for MachO.
    #[serde(rename = "bcsymbolmap")]
    BcSymbolMap,
    /// The il2cpp `LineNumberMapping.json` file.
    ///
    /// This file maps from C++ source locations to the original C# source location it was transpiled from.
    #[serde(rename = "il2cpp")]
    Il2cpp,
    /// A proguard debug file.
    Proguard,
}

impl FileType {
    /// Lists all available file types.
    #[inline]
    pub fn all() -> &'static [Self] {
        use FileType::*;
        &[
            Pdb,
            MachDebug,
            ElfDebug,
            Pe,
            MachCode,
            ElfCode,
            WasmCode,
            WasmDebug,
            Breakpad,
            SourceBundle,
            UuidMap,
            BcSymbolMap,
            PortablePdb,
            Proguard,
        ]
    }

    /// Source providing file types.
    #[inline]
    pub fn sources() -> &'static [Self] {
        &[FileType::SourceBundle, FileType::PortablePdb]
    }

    /// Given an object type, returns filetypes in the order they should be tried.
    #[inline]
    pub fn from_object_type(ty: ObjectType) -> &'static [Self] {
        match ty {
            // There are instances where an application's debug files are ELFs despite the
            // executable not being ELFs themselves. It probably isn't correct to assume that any
            // specific debug file type is heavily coupled with a particular executable type so we
            // return a union of all possible debug file types for native applications.
            ObjectType::Macho => &[
                FileType::MachCode,
                FileType::Breakpad,
                FileType::MachDebug,
                FileType::Pdb,
                FileType::ElfDebug,
            ],
            ObjectType::Pe => &[
                FileType::Pe,
                FileType::Breakpad,
                FileType::MachDebug,
                FileType::Pdb,
                FileType::ElfDebug,
            ],
            ObjectType::Elf => &[
                FileType::ElfCode,
                FileType::Breakpad,
                FileType::MachDebug,
                FileType::Pdb,
                FileType::ElfDebug,
            ],
            ObjectType::Wasm => &[FileType::WasmCode, FileType::WasmDebug],
            ObjectType::PeDotnet => &[FileType::PortablePdb],
            _ => Self::all(),
        }
    }
}

impl AsRef<str> for FileType {
    fn as_ref(&self) -> &str {
        match *self {
            FileType::Pe => "pe",
            FileType::Pdb => "pdb",
            FileType::MachDebug => "mach_debug",
            FileType::MachCode => "mach_code",
            FileType::ElfDebug => "elf_debug",
            FileType::ElfCode => "elf_code",
            FileType::WasmDebug => "wasm_debug",
            FileType::WasmCode => "wasm_code",
            FileType::Breakpad => "breakpad",
            FileType::SourceBundle => "sourcebundle",
            FileType::UuidMap => "uuidmap",
            FileType::BcSymbolMap => "bcsymbolmap",
            FileType::Il2cpp => "il2cpp",
            FileType::PortablePdb => "portablepdb",
            FileType::Proguard => "proguard",
        }
    }
}
