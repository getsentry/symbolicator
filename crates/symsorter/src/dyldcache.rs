use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use object::read::macho::DyldCacheImage;
use object::write;
use object::{
    Object, ObjectSection, ObjectSegment, ObjectSymbol, RelocationTarget, SymbolFlags, SymbolKind,
    SymbolSection,
};
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, Symbol};

use crate::config::RunConfig;

// note: apple's dyld extractor zeroes out all dyld info (LC_DYLD_INFO_ONLY).
// maybe if we decide to preserve its contents when we're manually extracting
// these images we could get a little bit more extra info?
pub(crate) fn extract_image(image: DyldCacheImage) -> Result<write::Object> {
    let in_obj = image.parse_object()?;

    let mut out_obj =
        write::Object::new(in_obj.format(), in_obj.architecture(), in_obj.endianness());
    out_obj.mangling = write::Mangling::None;
    out_obj.flags = in_obj.flags();

    match in_obj.mach_uuid() {
        Ok(Some(uuid)) => out_obj.set_uuid(uuid),
        Ok(None) => anyhow::bail!("image is missing a UUID"),
        Err(e) => anyhow::bail!("unable to find a UUID for image {}", e),
    }?;

    let mut trimmed_sections = HashSet::new();
    let mut out_sections = HashMap::new();
    let mut first_symbol_offset = None;
    for in_section in in_obj.sections() {
        let segment_name = match in_section.segment_name() {
            Ok(Some(name)) => name,
            Ok(None) | Err(_) => {
                log!(
                    "a section at index {:?} is missing a segment name, skipping",
                    in_section.index(),
                );
                trimmed_sections.insert(in_section.index());
                continue;
            }
        };
        if segment_name != "__TEXT" {
            trimmed_sections.insert(in_section.index());
            continue;
        }
        // If the name itself is empty, let Sentry figure out what to do with it
        let section_name = match in_section.name() {
            Ok(name) => name,
            Err(_) => {
                log!(
                    "a section at index {:?} in segment {} is missing a section name, skipping",
                    in_section.index(),
                    segment_name,
                );
                trimmed_sections.insert(in_section.index());
                continue;
            }
        };
        if first_symbol_offset.is_none() {
            let sec_address = in_section.address();
            let seg_address = in_obj
                .segments()
                .find(|s| matches!(s.name(), Ok(Some(name)) if name == segment_name))
                .map(|s| s.address());

            match seg_address {
                Some(seg_addr) => {
                    first_symbol_offset = Some(sec_address - seg_addr);
                }
                None => {
                    log!(
                        "section {} (index: {:?}) in segment {} is missing an address, skipping",
                        section_name,
                        in_section.index(),
                        segment_name,
                    );
                    trimmed_sections.insert(in_section.index());
                    continue;
                }
            }
        }
        let section_id = out_obj.add_section(
            segment_name.as_bytes().to_vec(),
            section_name.as_bytes().to_vec(),
            in_section.kind(),
        );
        let out_section = out_obj.section_mut(section_id);
        // No need to check if this is a zerofill section because those are exclusive to __DATA
        // segments
        out_section.set_data(in_section.data().unwrap().into(), in_section.align());
        out_section.flags = in_section.flags();
        // TODO: Do we need to realign the section indices?
        // If so, we're going to need a mapping of old indices to new indices for symbol
        // mapping
        out_sections.insert(in_section.index(), section_id);
    }

    if let Some(offset) = first_symbol_offset {
        out_obj.set_symbol_start_vmaddr(offset)?;
    }

    // dyld_shared_cache_util does some merging of symbols from LINKEDIN into symtab which we're
    // not doing here. I haven't found any evidence that we've lost any symbols from not doing the
    // same.
    let mut out_symbols = HashMap::new();
    for in_symbol in in_obj.symbols() {
        match in_symbol.kind() {
            SymbolKind::Unknown => continue,
            // Null and Label are unsupported according to object::macho_write()
            SymbolKind::Null => continue,
            SymbolKind::Label => continue,
            _ => (),
        }
        let (section, value) = match in_symbol.section() {
            SymbolSection::None => (write::SymbolSection::None, in_symbol.address()),
            SymbolSection::Undefined => (write::SymbolSection::Undefined, in_symbol.address()),
            SymbolSection::Absolute => (write::SymbolSection::Absolute, in_symbol.address()),
            SymbolSection::Common => (write::SymbolSection::Common, in_symbol.address()),
            SymbolSection::Section(index) => {
                if let Some(out_section) = out_sections.get(&index) {
                    (
                        write::SymbolSection::Section(*out_section),
                        in_symbol.address() - in_obj.section_by_index(index).unwrap().address(),
                    )
                } else {
                    // Ignore symbols for sections that we're not interested in, i.e. ones that
                    // don't belong to __TEXT.
                    if trimmed_sections.contains(&index) {
                        continue;
                    }

                    // Ignore symbols for sections that we have skipped.
                    assert_eq!(in_symbol.kind(), SymbolKind::Section);
                    continue;
                }
            }
            _ => anyhow::bail!("unknown symbol section for {:?}", in_symbol),
        };
        let flags = match in_symbol.flags() {
            SymbolFlags::None => SymbolFlags::None,
            SymbolFlags::MachO { n_desc } => SymbolFlags::MachO { n_desc },
            SymbolFlags::Elf { .. } => {
                anyhow::bail!(
                    "encountered elf symbol flags for {:?} in a dyld cache image",
                    in_symbol
                )
            }
            SymbolFlags::CoffSection { .. } => {
                anyhow::bail!(
                    "encountered coff symbol flags for {:?} in a dyld cache image",
                    in_symbol
                )
            }
            _ => anyhow::bail!("unknown symbol flags for {:?}", in_symbol),
        };
        let out_symbol = write::Symbol {
            name: in_symbol
                .name()
                .unwrap_or("nameless symbol")
                .as_bytes()
                .to_vec(),
            value,
            size: in_symbol.size(),
            kind: in_symbol.kind(),
            scope: in_symbol.scope(),
            weak: in_symbol.is_weak(),
            section,
            flags,
        };
        let symbol_id = out_obj.add_symbol(out_symbol);
        // TODO: Do we need to realign the symbol indices?
        out_symbols.insert(in_symbol.index(), symbol_id);
    }

    for in_section in in_obj.sections() {
        let segment_name = match in_section.segment_name() {
            Ok(Some(name)) => name,
            Ok(None) | Err(_) => {
                log!(
                    "unable to find or generate a valid segment name for section \"{}\"",
                    in_section.name().unwrap_or("nameless section"),
                );
                continue;
            }
        };
        if segment_name != "__TEXT" || in_section.name().is_err() {
            continue;
        }
        let out_section = match out_sections.get(&in_section.index()) {
            Some(section) => section,
            None => {
                log!(
                    "searched for a __TEXT section that was filtered out; skipping. Name: {:?} Index: {:?}",
                    in_section.name(),
                    in_section.index()
                );
                continue;
            }
        };
        for (offset, in_relocation) in in_section.relocations() {
            let symbol = match in_relocation.target() {
                RelocationTarget::Symbol(symbol) => *out_symbols.get(&symbol).unwrap(),
                RelocationTarget::Section(section) => {
                    out_obj.section_symbol(*out_sections.get(&section).unwrap())
                }
                _ => {
                    log!("unknown relocation target for {:?}", in_relocation);
                    continue;
                }
            };
            let out_relocation = write::Relocation {
                offset,
                size: in_relocation.size(),
                kind: in_relocation.kind(),
                encoding: in_relocation.encoding(),
                symbol,
                addend: in_relocation.addend(),
            };
            out_obj
                .add_relocation(*out_section, out_relocation)
                .unwrap();
        }
    }

    Ok(out_obj)
}

pub(crate) fn check_against_dsc(path: &str, rare_image: &[u8]) -> Result<()> {
    let emitted_obj = Archive::parse(&rare_image)
        .map_err(|e| anyhow!("failed to parse archive {}", e))?
        .objects()
        .next()
        .unwrap()
        .map_err(|e| anyhow!("couldn't grab object from archive {}", e))?;
    let emitted_symbols = emitted_obj.symbol_map();

    let mut applegen_path: PathBuf = PathBuf::from("/Users/betty/shared-cache-libraries/x86_64");
    let (_, nonabspath) = path.split_once('/').unwrap();
    applegen_path.push(nonabspath);
    let raw_apple = ByteView::open(&applegen_path)
        .map_err(|e| anyhow!("couldn't open file at {:?} {}", &applegen_path, e))?;
    let applegen_obj = Archive::parse(&raw_apple)?
        .objects()
        .next()
        .unwrap()
        .map_err(|e| anyhow!("couldn't grab object from archive {}", e))?;
    let applegen_symbols = applegen_obj.symbol_map();

    match (emitted_symbols.len(), applegen_symbols.len()) {
        (0, 0) => {}
        (0, _) => {
            log!("all symbols missing from emitted");
            return Ok(());
        }
        (_, 0) => {
            log!("applegen has no symbols but emitted does?");
            return Ok(());
        }
        (_, _) => {}
    }

    let mut missing: Vec<(String, &Symbol, Option<&Symbol>)> = Vec::new();
    for search in applegen_symbols.iter() {
        if let Some(found) = emitted_symbols.lookup(search.address) {
            // look, ma! fizzbuzz!
            let mismatch = match (found.name() != search.name(), found.size != search.size) {
                (true, true) => "name, size",
                (true, false) => "name",
                (false, true) => "size",
                (false, false) => "",
            }
            .to_string();
            if !mismatch.is_empty() {
                missing.push((format!("applegen ({})", mismatch), search, Some(found)));
            }
        } else {
            missing.push(("applegen".to_string(), search, None))
        }
    }

    for search in emitted_symbols.iter() {
        if let Some(found) = applegen_symbols.lookup(search.address) {
            let mismatch = match (found.name() != search.name(), found.size != search.size) {
                (true, true) => "name, size",
                (true, false) => "name",
                (false, true) => "size",
                (false, false) => "",
            }
            .to_string();
            if !mismatch.is_empty() {
                missing.push((format!("emitted ({})", mismatch), search, Some(found)));
            }
        } else {
            missing.push(("emitted".to_string(), search, None))
        }
    }

    if missing.is_empty() {
        log!("all symbols accounted for in {}", path);
    } else {
        log!("missing/mismatched symbols: {:#?}", missing);
    }
    Ok(())
}
