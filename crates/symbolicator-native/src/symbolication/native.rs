use symbolic::common::{split_path, InstructionInfo, Language};
use symbolic::symcache::SymCache;
use symbolicator_service::metric;
use symbolicator_service::utils::hex::HexValue;

use crate::interface::{
    AdjustInstructionAddr, FrameStatus, RawFrame, Registers, Signal, SymbolicatedFrame,
};

use super::demangle::{demangle_symbol, DemangleCache};
use super::module_lookup::CacheLookupResult;

pub fn symbolicate_native_frame(
    demangle_cache: &DemangleCache,
    symcache: &SymCache,
    lookup_result: CacheLookupResult,
    relative_addr: u64,
    frame: &RawFrame,
    index: usize,
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
                trust: frame.trust,
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
                    tracing::warn!(
                            "Underflow when trying to subtract image start addr from caller address after heuristics"
                        );
                    metric!(counter("relative_addr.underflow") += 1);
                    FrameStatus::MissingSymbol
                })
        } else {
            Ok(addr)
        }
    } else {
        tracing::warn!("Underflow when trying to subtract image start addr from caller address before heuristics");
        metric!(counter("relative_addr.underflow") += 1);
        Err(FrameStatus::MissingSymbol)
    }
}
