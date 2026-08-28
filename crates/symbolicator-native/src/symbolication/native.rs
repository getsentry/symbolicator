use std::collections::BTreeMap;

use minidump::CpuContext;
use symbolic::common::{CpuFamily, InstructionInfo, Language, split_path};
use symbolic::symcache::{SourceLocation, SymCache, Type, Variable, VariableLocation};
use symbolicator_service::metric;
use symbolicator_service::utils::hex::HexValue;

use crate::interface::{
    AdjustInstructionAddr, FrameStatus, RawFrame, Registers, Signal, SymbolicatedFrame,
};

use super::demangle::{DemangleCache, demangle_symbol};
use super::module_lookup::CacheLookupResult;

pub fn symbolicate_native_frame(
    demangle_cache: &DemangleCache,
    symcache: &SymCache,
    lookup_result: CacheLookupResult,
    relative_addr: u64,
    frame: &RawFrame,
    index: usize,
    extract_variables: bool,
) -> Result<Vec<SymbolicatedFrame>, FrameStatus> {
    tracing::trace!("Symbolicating {:#x}", relative_addr);
    let mut rv = vec![];

    // The symbol addr only makes sense for the outermost top-level function, and not its inlinees.
    // We keep track of it while iterating and only set it for the last frame,
    // which is the top-level function.
    let mut sym_addr = None;
    let instruction_addr = HexValue(lookup_result.expose_preferred_addr(relative_addr));

    for source_location in symcache.lookup(relative_addr) {
        let abs_path = source_location
            .file()
            .map(|f| f.full_path())
            .unwrap_or_default();
        let filename = split_path(&abs_path).1;

        let func = source_location.function();
        let (symbol, function) = demangle_symbol(demangle_cache, &func);

        sym_addr = Some(HexValue(
            lookup_result.expose_preferred_addr(func.entry_pc() as u64),
        ));
        let filename = if !filename.is_empty() {
            Some(filename.to_string())
        } else {
            frame.filename.clone()
        };

        let mut vars = None;
        if extract_variables {
            vars = do_extract_variables(&source_location, symcache, &frame.registers);
        }

        rv.push(SymbolicatedFrame {
            status: FrameStatus::Symbolicated,
            original_index: Some(index),
            raw: RawFrame {
                platform: frame.platform.clone(),
                package: lookup_result.object_info.raw.code_file.clone(),
                addr_mode: lookup_result.preferred_addr_mode(),
                instruction_addr,
                adjust_instruction_addr: frame.adjust_instruction_addr,
                function_id: frame.function_id,
                symbol: Some(symbol),
                abs_path: if !abs_path.is_empty() {
                    Some(abs_path)
                } else {
                    frame.abs_path.clone()
                },
                function: Some(function),
                filename,
                lineno: Some(source_location.line()),
                pre_context: vec![],
                context_line: None,
                post_context: vec![],
                source_link: None,
                sym_addr: None,
                lang: match func.language() {
                    Language::Unknown => None,
                    language => Some(language),
                },
                in_app: None,
                vars,
                trust: frame.trust,
                registers: Default::default(),
            },
        });
    }

    if let Some(last_frame) = rv.last_mut() {
        last_frame.raw.sym_addr = sym_addr;
    }

    if rv.is_empty() {
        return Err(FrameStatus::MissingSymbol);
    }

    Ok(rv)
}

pub fn get_relative_caller_addr(
    symcache: &SymCache,
    lookup_result: &CacheLookupResult,
    registers: &Registers,
    signal: Option<Signal>,
    index: usize,
    adjustment: AdjustInstructionAddr,
) -> Result<u64, FrameStatus> {
    if let Some(addr) = lookup_result.relative_addr {
        // heuristics currently are only supported when we can work with absolute addresses.
        // In cases where this is not possible we skip this part entirely and use the relative
        // address calculated by the lookup result as lookup address in the module.
        if let Some(absolute_addr) = lookup_result.object_info.rel_to_abs_addr(addr) {
            let is_crashing_frame = index == 0;
            let ip_register_value = if is_crashing_frame {
                symcache
                    .arch()
                    .cpu_family()
                    .ip_register_name()
                    .and_then(|ip_reg_name| registers.get(ip_reg_name))
                    .map(|x| x.0)
            } else {
                None
            };

            let mut instruction_info = InstructionInfo::new(symcache.arch(), absolute_addr);
            let instruction_info = instruction_info
                .is_crashing_frame(is_crashing_frame)
                .signal(signal.map(|signal| signal.0))
                .ip_register_value(ip_register_value);

            let absolute_caller_addr = match adjustment {
                AdjustInstructionAddr::Yes => instruction_info.previous_address(),
                AdjustInstructionAddr::No => instruction_info.aligned_address(),
                AdjustInstructionAddr::Auto => instruction_info.caller_address(),
            };

            lookup_result
                .object_info
                .abs_to_rel_addr(absolute_caller_addr)
                .ok_or_else(|| {
                    tracing::debug!(
                        "Underflow when trying to subtract image start addr from caller address after heuristics"
                    );
                    metric!(counter("relative_addr.underflow") += 1);
                    FrameStatus::MissingSymbol
                })
        } else {
            Ok(addr)
        }
    } else {
        tracing::debug!(
            "Underflow when trying to subtract image start addr from caller address before heuristics"
        );
        metric!(counter("relative_addr.underflow") += 1);
        Err(FrameStatus::MissingSymbol)
    }
}

fn do_extract_variables<'data, 'cache>(
    source_location: &SourceLocation<'data, 'cache>,
    cache: &SymCache<'cache>,
    registers: &Registers,
) -> Option<BTreeMap<String, serde_json::Value>> {
    let mut result = BTreeMap::new();

    for variable in source_location.variables() {
        let Some(name) = variable.name() else {
            continue;
        };

        let mut ty = String::new();
        resolve_type_name(&mut ty, cache, variable.ty(), 0);

        let value = resolve_variable_value(cache, registers, &variable);

        // This doesn't handle name collisions currently.
        result.insert(
            name.to_owned(),
            match value {
                Some(value) => format!("{value} ({ty})").into(),
                None => ty.into(),
            },
        );
    }

    match result.is_empty() {
        false => Some(result),
        true => None,
    }
}

fn resolve_variable_value(
    cache: &SymCache<'_>,
    registers: &Registers,
    variable: &Variable<'_, '_>,
) -> Option<HexValue> {
    variable.locations().find_map(|location| {
        let VariableLocation::Register { id } = location.location else {
            return None;
        };

        // Temporary hack, `symbolic` will need an abstraction over registers, which allows
        // mapping register names to the gimli register ids.
        match cache.arch().cpu_family() {
            CpuFamily::Amd64 => minidump::format::CONTEXT_AMD64::REGISTERS,
            CpuFamily::Arm64 => minidump::format::CONTEXT_ARM64::REGISTERS,
            _ => &[],
        }
        .get(id as usize)
        .and_then(|&reg| registers.get(reg))
        .copied()
    })
}

fn resolve_type_name(
    result: &mut String,
    cache: &SymCache<'_>,
    ty: Option<Type<'_>>,
    depth: usize,
) {
    let Some(ty) = ty else {
        result.push_str("<unknown>");
        return;
    };

    // This really is just temporary and not even necessary, the current depth limit in symbolic is 5.
    // With more changes we'll have to solve this properly. As we're also going to have to resolve
    // the variable contents, not just a type name.
    if depth > 10 {
        return;
    }

    match ty {
        Type::Primitive(primitive) => result.push_str(primitive.name().unwrap_or("")),
        Type::Pointer(pointer) if pointer.pointee().is_none() => result.push_str("void*"),
        Type::Pointer(pointer) => {
            let ty = pointer.pointee().and_then(|p| cache.lookup_type(p));
            resolve_type_name(result, cache, ty, depth);
            result.push('*');
        }
        _ => result.push_str("<not implemented>"),
    }
}
