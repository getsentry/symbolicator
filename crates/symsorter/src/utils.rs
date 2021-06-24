// use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use lazy_static::lazy_static;
// use object::{
//     write, Object as GimliObject, ObjectComdat, ObjectSection, ObjectSymbol, RelocationTarget,
//     SectionKind, SymbolFlags, SymbolKind, SymbolSection,
// };
use regex::Regex;
use symbolic::common::ByteView;
// use symbolic::common::{ByteView, CodeId, Uuid};
use symbolic::debuginfo::sourcebundle::SourceBundleWriter;
use symbolic::debuginfo::{Archive, FileFormat, Object, ObjectKind};

lazy_static! {
    static ref BAD_CHARS_RE: Regex = Regex::new(r"[^a-zA-Z0-9.,-]+").unwrap();
}

/// Makes a safe bundle ID from arbitrary input.
pub fn make_bundle_id(input: &str) -> String {
    BAD_CHARS_RE
        .replace_all(input, "_")
        .trim_matches(&['_'][..])
        .to_string()
}

/// Checks if something is a safe bundle id
pub fn is_bundle_id(input: &str) -> bool {
    make_bundle_id(input) == input
}

/// Gets the unified ID from an object.
pub fn get_unified_id(obj: &Object) -> String {
    if obj.file_format() == FileFormat::Pe || obj.code_id().is_none() {
        obj.debug_id().breakpad().to_string().to_lowercase()
    } else {
        obj.code_id().as_ref().unwrap().as_str().to_string()
    }
}

/// Returns the intended target filename for an object.
pub fn get_target_filename(obj: &Object) -> Option<PathBuf> {
    let id = get_unified_id(obj);
    // match the unified format here.
    let suffix = match obj.kind() {
        ObjectKind::Debug => "debuginfo",
        ObjectKind::Sources => {
            if obj.file_format() == FileFormat::SourceBundle {
                "sourcebundle"
            } else {
                return None;
            }
        }
        ObjectKind::Relocatable | ObjectKind::Library | ObjectKind::Executable => "executable",
        _ => return None,
    };
    Some(format!("{}/{}/{}", &id[..2], &id[2..], suffix).into())
}

// pub fn get_dyld_image_unified_id(obj: &object::File) -> String {
//     let uuid = &obj
//         .mach_uuid()
//         .unwrap_or_default()
//         .and_then(|slice| Uuid::from_slice(slice.as_ref()).ok())
//         .unwrap_or_default();
//     CodeId::from_binary(&uuid.as_bytes()[..]).to_string()
// }

// pub fn get_dyld_image_filename(obj: &object::File, kind: &ObjectKind) -> Option<PathBuf> {
//     let id = get_dyld_image_unified_id(obj);
//     let suffix = match kind {
//         ObjectKind::Debug => "debuginfo",
//         ObjectKind::Relocatable | ObjectKind::Library | ObjectKind::Executable => "executable",
//         _ => return None,
//     };

//     Some(format!("{}/{}/{}", &id[..2], &id[2..], suffix).into())
// }

/// Creates a source bundle from a path.
pub fn create_source_bundle(path: &Path, unified_id: &str) -> Result<Option<ByteView<'static>>> {
    let bv = ByteView::open(path)?;
    let archive = Archive::parse(&bv).map_err(|e| anyhow!(e))?;
    for obj in archive.objects() {
        let obj = obj.map_err(|e| anyhow!(e))?;
        if get_unified_id(&obj) == unified_id {
            let mut out = Vec::<u8>::new();
            let writer =
                SourceBundleWriter::start(Cursor::new(&mut out)).map_err(|e| anyhow!(e))?;
            let name = path.file_name().unwrap().to_string_lossy();
            if writer.write_object(&obj, &name).map_err(|e| anyhow!(e))? {
                return Ok(Some(ByteView::from_vec(out)));
            }
        }
    }
    Ok(None)
}

/// Console logging for the symsorter app.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        {
            if (!RunConfig::get().quiet) {
                println!($($arg)*);
            }
        }
    }
}

// pub fn clone_dyld_image(in_object: &object::File) -> Vec<u8> {
//     println!("format: {:?}", in_object.format());
//     let mut out_object = write::Object::new(
//         in_object.format(),
//         in_object.architecture(),
//         in_object.endianness(),
//     );
//     out_object.mangling = write::Mangling::None;
//     out_object.flags = in_object.flags();

//     let mut out_sections = HashMap::new();
//     for in_section in in_object.sections() {
//         if in_section.kind() == SectionKind::Metadata {
//             continue;
//         }
//         let section_id = out_object.add_section(
//             in_section
//                 .segment_name()
//                 .unwrap()
//                 .unwrap_or("")
//                 .as_bytes()
//                 .to_vec(),
//             in_section.name().unwrap().as_bytes().to_vec(),
//             in_section.kind(),
//         );
//         let out_section = out_object.section_mut(section_id);
//         if out_section.is_bss() {
//             out_section.append_bss(in_section.size(), in_section.align());
//         } else {
//             out_section.set_data(in_section.data().unwrap().into(), in_section.align());
//         }
//         out_section.flags = in_section.flags();
//         out_sections.insert(in_section.index(), section_id);
//     }

//     let mut out_symbols = HashMap::new();
//     for in_symbol in in_object.symbols() {
//         match in_symbol.kind() {
//             SymbolKind::Unknown => continue,
//             // null and label are unsupported according to macho_write()
//             SymbolKind::Null => continue,
//             SymbolKind::Label => continue,
//             _ => (),
//         }
//         let (section, value) = match in_symbol.section() {
//             SymbolSection::None => (write::SymbolSection::None, in_symbol.address()),
//             SymbolSection::Undefined => (write::SymbolSection::Undefined, in_symbol.address()),
//             SymbolSection::Absolute => (write::SymbolSection::Absolute, in_symbol.address()),
//             SymbolSection::Common => (write::SymbolSection::Common, in_symbol.address()),
//             SymbolSection::Section(index) => {
//                 if let Some(out_section) = out_sections.get(&index) {
//                     (
//                         write::SymbolSection::Section(*out_section),
//                         in_symbol.address() - in_object.section_by_index(index).unwrap().address(),
//                     )
//                 } else {
//                     // Ignore symbols for sections that we have skipped.
//                     assert_eq!(in_symbol.kind(), SymbolKind::Section);
//                     continue;
//                 }
//             }
//             _ => panic!("unknown symbol section for {:?}", in_symbol),
//         };
//         let flags = match in_symbol.flags() {
//             SymbolFlags::None => SymbolFlags::None,
//             SymbolFlags::Elf { st_info, st_other } => SymbolFlags::Elf { st_info, st_other },
//             SymbolFlags::MachO { n_desc } => SymbolFlags::MachO { n_desc },
//             SymbolFlags::CoffSection {
//                 selection,
//                 associative_section,
//             } => {
//                 let associative_section =
//                     associative_section.map(|index| *out_sections.get(&index).unwrap());
//                 SymbolFlags::CoffSection {
//                     selection,
//                     associative_section,
//                 }
//             }
//             _ => panic!("unknown symbol flags for {:?}", in_symbol),
//         };
//         let out_symbol = write::Symbol {
//             name: in_symbol.name().unwrap_or("").as_bytes().to_vec(),
//             value,
//             size: in_symbol.size(),
//             kind: in_symbol.kind(),
//             scope: in_symbol.scope(),
//             weak: in_symbol.is_weak(),
//             section,
//             flags,
//         };
//         let symbol_id = out_object.add_symbol(out_symbol);
//         out_symbols.insert(in_symbol.index(), symbol_id);
//     }

//     for in_section in in_object.sections() {
//         if in_section.kind() == SectionKind::Metadata {
//             continue;
//         }
//         let out_section = *out_sections.get(&in_section.index()).unwrap();
//         for (offset, in_relocation) in in_section.relocations() {
//             let symbol = match in_relocation.target() {
//                 RelocationTarget::Symbol(symbol) => *out_symbols.get(&symbol).unwrap(),
//                 RelocationTarget::Section(section) => {
//                     out_object.section_symbol(*out_sections.get(&section).unwrap())
//                 }
//                 _ => panic!("unknown relocation target for {:?}", in_relocation),
//             };
//             let out_relocation = write::Relocation {
//                 offset,
//                 size: in_relocation.size(),
//                 kind: in_relocation.kind(),
//                 encoding: in_relocation.encoding(),
//                 symbol,
//                 addend: in_relocation.addend(),
//             };
//             out_object
//                 .add_relocation(out_section, out_relocation)
//                 .unwrap();
//         }
//     }

//     for in_comdat in in_object.comdats() {
//         let mut sections = Vec::new();
//         for in_section in in_comdat.sections() {
//             sections.push(*out_sections.get(&in_section).unwrap());
//         }
//         out_object.add_comdat(write::Comdat {
//             kind: in_comdat.kind(),
//             symbol: *out_symbols.get(&in_comdat.symbol()).unwrap(),
//             sections,
//         });
//     }

//     out_object.write().unwrap()
// }
