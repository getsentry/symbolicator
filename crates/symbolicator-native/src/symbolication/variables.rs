use std::sync::Arc;

use symbolic::common::ByteView;

use crate::interface::{RawFrame, Variable};
use super::module_lookup::ModuleLookup;

use gimli::{self, Endianity, Reader as _};
use object::Object as _;

/// The `gimli` reader type we use throughout this module.
///
/// `EndianArcSlice` owns its bytes via `Arc<[u8]>` rather than borrowing them, so parsed
/// `Dwarf`/`Unit` values have no lifetime tied to the original object file and can be cached
/// and reused across frames.
type R = gimli::EndianArcSlice<gimli::RunTimeEndian>;
type Dwarf = gimli::Dwarf<R>;
type Unit = gimli::Unit<R>;
type AttrValue = gimli::AttributeValue<R>;

/// A safe reader over a minidump's memory regions, built once per request and reused for
/// every frame instead of re-fetching the memory stream on every single read.
#[derive(Clone, Copy)]
struct MemoryReader<'a, 'md> {
    memory_list: Option<&'a minidump::MinidumpMemoryList<'md>>,
    memory64_list: Option<&'a minidump::MinidumpMemory64List<'md>>,
}

impl MemoryReader<'_, '_> {
    fn read(&self, addr: u64, size: usize) -> Option<Vec<u8>> {
        if let Some(memory_list) = self.memory_list {
            for block in memory_list.iter() {
                if addr >= block.base_address && addr < block.base_address + block.size {
                    let offset = (addr - block.base_address) as usize;
                    let available = (block.size as usize).saturating_sub(offset);
                    let to_read = std::cmp::min(size, available);
                    return Some(block.bytes[offset..offset + to_read].to_vec());
                }
            }
        }
        if let Some(memory64_list) = self.memory64_list {
            for block in memory64_list.iter() {
                if addr >= block.base_address && addr < block.base_address + block.size {
                    let offset = (addr - block.base_address) as usize;
                    let available = (block.size as usize).saturating_sub(offset);
                    let to_read = std::cmp::min(size, available);
                    return Some(block.bytes[offset..offset + to_read].to_vec());
                }
            }
        }
        None
    }
}

/// Decode up to 8 bytes as an unsigned integer using the given endianness.
///
/// Bounded to 8 bytes so a larger-than-expected DWARF piece (e.g. a SIMD register) can't
/// shift a `u64` by more than 63 bits, which would panic.
fn decode_uint(bytes: &[u8], endian: gimli::RunTimeEndian) -> u64 {
    let mut val = 0u64;
    for (i, &b) in bytes.iter().enumerate().take(8) {
        if endian.is_little_endian() {
            val |= (b as u64) << (i * 8);
        } else {
            val = (val << 8) | (b as u64);
        }
    }
    val
}

/// Translate DWARF register indices to name strings for different CPU architectures.
fn get_register_name(arch: object::Architecture, reg: gimli::Register) -> Option<&'static str> {
    match arch {
        object::Architecture::X86_64 => match reg.0 {
            0 => Some("rax"),
            1 => Some("rdx"),
            2 => Some("rcx"),
            3 => Some("rbx"),
            4 => Some("rsi"),
            5 => Some("rdi"),
            6 => Some("rbp"),
            7 => Some("rsp"),
            8 => Some("r8"),
            9 => Some("r9"),
            10 => Some("r10"),
            11 => Some("r11"),
            12 => Some("r12"),
            13 => Some("r13"),
            14 => Some("r14"),
            15 => Some("r15"),
            16 => Some("rip"),
            _ => None,
        },
        object::Architecture::I386 => match reg.0 {
            0 => Some("eax"),
            1 => Some("ecx"),
            2 => Some("edx"),
            3 => Some("ebx"),
            4 => Some("esp"),
            5 => Some("ebp"),
            6 => Some("esi"),
            7 => Some("edi"),
            8 => Some("eip"),
            _ => None,
        },
        object::Architecture::Arm => match reg.0 {
            0..=12 => Some(match reg.0 {
                0 => "r0", 1 => "r1", 2 => "r2", 3 => "r3", 4 => "r4",
                5 => "r5", 6 => "r6", 7 => "r7", 8 => "r8", 9 => "r9",
                10 => "r10", 11 => "r11", 12 => "r12", _ => unreachable!(),
            }),
            13 => Some("sp"),
            14 => Some("lr"),
            15 => Some("pc"),
            _ => None,
        },
        object::Architecture::Aarch64 => match reg.0 {
            0..=28 => Some(match reg.0 {
                0 => "x0", 1 => "x1", 2 => "x2", 3 => "x3", 4 => "x4",
                5 => "x5", 6 => "x6", 7 => "x7", 8 => "x8", 9 => "x9",
                10 => "x10", 11 => "x11", 12 => "x12", 13 => "x13", 14 => "x14",
                15 => "x15", 16 => "x16", 17 => "x17", 18 => "x18", 19 => "x19",
                20 => "x20", 21 => "x21", 22 => "x22", 23 => "x23", 24 => "x24",
                25 => "x25", 26 => "x26", 27 => "x27", 28 => "x28",
                _ => unreachable!(),
            }),
            // rust-minidump's `valid_registers()` names x29 "fp" (its canonical AArch64
            // frame-pointer register), not "x29" -- see `CONTEXT_ARM64::REGISTERS` in
            // minidump/src/context.rs. `frame.registers` is keyed by that canonical name, so
            // returning "x29" here would make every `frame.registers.get(reg_name)` lookup for
            // this (extremely common, since it's the DWARF frame-base register on AArch64) miss
            // and silently report the variable as optimized out.
            29 => Some("fp"),
            30 => Some("lr"),
            31 => Some("sp"),
            32 => Some("pc"),
            _ => None,
        },
        _ => None,
    }
}

/// Follows `DW_AT_abstract_origin` (inlined instances) or `DW_AT_specification` (out-of-line
/// definitions of previously-declared entities) to the DIE that actually carries a name/type.
///
/// Compilers commonly emit concrete `DW_TAG_formal_parameter`/`DW_TAG_variable` DIEs inside a
/// `DW_TAG_inlined_subroutine` with only a `DW_AT_location` (which varies per call site) and a
/// reference to the abstract DIE that holds the actual `DW_AT_name`/`DW_AT_type`. Without
/// following this reference, every variable inside an inlined frame silently has no name and
/// gets dropped.
fn resolve_origin<'u>(unit: &'u Unit, die: &gimli::DebuggingInformationEntry<R>) -> Option<gimli::DebuggingInformationEntry<'u, 'u, R>> {
    let attr = die
        .attr_value(gimli::DW_AT_abstract_origin)
        .ok()
        .flatten()
        .or_else(|| die.attr_value(gimli::DW_AT_specification).ok().flatten())?;
    match attr {
        gimli::AttributeValue::UnitRef(offset) => unit.entry(offset).ok(),
        _ => None,
    }
}

/// Caps recursion/iteration in the name- and type-resolution helpers below and in
/// [`format_piece_value`], so a malformed or adversarial `DW_AT_type`/`DW_AT_abstract_origin`
/// cycle (which real compiler output can't produce, but a crafted/corrupted debug file could)
/// can't cause unbounded recursion or an infinite loop — both are process-ending in Rust (stack
/// overflow aborts, and a hung loop pins a worker thread), not just an in-band error.
const MAX_TYPE_FORMATTING_DEPTH: u32 = 16;

fn get_die_name(
    dwarf: &Dwarf,
    unit: &Unit,
    die: &gimli::DebuggingInformationEntry<R>,
) -> Option<String> {
    let mut current = die.clone();
    for _ in 0..MAX_TYPE_FORMATTING_DEPTH {
        if let Some(attr) = current.attr_value(gimli::DW_AT_name).ok().flatten() {
            return dwarf
                .attr_string(unit, attr)
                .ok()
                .and_then(|r| r.to_string_lossy().ok().map(|s| s.into_owned()));
        }
        current = resolve_origin(unit, &current)?;
    }
    None
}

fn get_type_name(dwarf: &Dwarf, unit: &Unit, die: &gimli::DebuggingInformationEntry<R>) -> String {
    get_type_name_impl(dwarf, unit, die, 0)
}

fn get_type_name_impl(dwarf: &Dwarf, unit: &Unit, die: &gimli::DebuggingInformationEntry<R>, depth: u32) -> String {
    if depth >= MAX_TYPE_FORMATTING_DEPTH {
        return "...".to_string();
    }
    if let Some(gimli::AttributeValue::UnitRef(offset)) = die.attr_value(gimli::DW_AT_type).ok().flatten() {
        if let Ok(type_die) = unit.entry(offset) {
            return format_type_impl(dwarf, unit, &type_die, depth + 1);
        }
    }
    if let Some(origin) = resolve_origin(unit, die) {
        return get_type_name_impl(dwarf, unit, &origin, depth + 1);
    }
    "void".to_string()
}

fn format_type_impl(dwarf: &Dwarf, unit: &Unit, die: &gimli::DebuggingInformationEntry<R>, depth: u32) -> String {
    if depth >= MAX_TYPE_FORMATTING_DEPTH {
        return "...".to_string();
    }
    match die.tag() {
        gimli::DW_TAG_base_type => {
            get_die_name(dwarf, unit, die).unwrap_or_else(|| "unknown_base".to_string())
        }
        gimli::DW_TAG_pointer_type => {
            let inner = get_type_name_impl(dwarf, unit, die, depth + 1);
            format!("{inner}*")
        }
        gimli::DW_TAG_reference_type => {
            let inner = get_type_name_impl(dwarf, unit, die, depth + 1);
            format!("{inner}&")
        }
        gimli::DW_TAG_const_type => {
            let inner = get_type_name_impl(dwarf, unit, die, depth + 1);
            format!("const {inner}")
        }
        gimli::DW_TAG_volatile_type => {
            let inner = get_type_name_impl(dwarf, unit, die, depth + 1);
            format!("volatile {inner}")
        }
        gimli::DW_TAG_typedef => {
            get_die_name(dwarf, unit, die).unwrap_or_else(|| get_type_name_impl(dwarf, unit, die, depth + 1))
        }
        gimli::DW_TAG_structure_type => {
            let name = get_die_name(dwarf, unit, die).unwrap_or_else(|| "struct".to_string());
            format!("struct {name}")
        }
        gimli::DW_TAG_class_type => get_die_name(dwarf, unit, die).unwrap_or_else(|| "class".to_string()),
        gimli::DW_TAG_union_type => {
            let name = get_die_name(dwarf, unit, die).unwrap_or_else(|| "union".to_string());
            format!("union {name}")
        }
        gimli::DW_TAG_array_type => {
            let inner = get_type_name_impl(dwarf, unit, die, depth + 1);
            format!("{inner}[]")
        }
        _ => get_die_name(dwarf, unit, die).unwrap_or_else(|| "unknown".to_string()),
    }
}

fn get_low_pc(die: &gimli::DebuggingInformationEntry<R>) -> Option<u64> {
    if let Some(gimli::AttributeValue::Addr(addr)) = die.attr_value(gimli::DW_AT_low_pc).ok().flatten() {
        Some(addr)
    } else {
        None
    }
}

fn get_high_pc(die: &gimli::DebuggingInformationEntry<R>, low_pc: u64) -> Option<u64> {
    match die.attr_value(gimli::DW_AT_high_pc).ok().flatten() {
        Some(gimli::AttributeValue::Addr(addr)) => Some(addr),
        Some(gimli::AttributeValue::Udata(size)) => Some(low_pc + size),
        _ => None,
    }
}

/// Returns the `[low, high)` PC ranges covered by `die`, from either a direct
/// `DW_AT_low_pc`/`DW_AT_high_pc` pair or (possibly several, for `-ffunction-sections`-style
/// layouts) `DW_AT_ranges` entries.
fn die_ranges(dwarf: &Dwarf, unit: &Unit, die: &gimli::DebuggingInformationEntry<R>) -> Vec<(u64, u64)> {
    if let Some(low_pc) = get_low_pc(die) {
        if let Some(high_pc) = get_high_pc(die, low_pc) {
            return vec![(low_pc, high_pc)];
        }
    }
    let mut out = vec![];
    if let Ok(Some(attr)) = die.attr(gimli::DW_AT_ranges) {
        if let Ok(Some(offset)) = dwarf.attr_ranges_offset(unit, attr.value()) {
            if let Ok(mut ranges) = dwarf.ranges(unit, offset) {
                while let Ok(Some(range)) = ranges.next() {
                    out.push((range.begin, range.end));
                }
            }
        }
    }
    out
}

fn die_contains_addr(dwarf: &Dwarf, unit: &Unit, die: &gimli::DebuggingInformationEntry<R>, addr: u64) -> bool {
    die_ranges(dwarf, unit, die)
        .into_iter()
        .any(|(low, high)| addr >= low && addr < high)
}

#[allow(clippy::too_many_arguments)]
fn evaluate_frame_base(
    dwarf: &Dwarf,
    unit: &Unit,
    attr: AttrValue,
    pc: u64,
    frame: &RawFrame,
    memory: MemoryReader,
    arch: object::Architecture,
    endian: gimli::RunTimeEndian,
) -> (String, String) {
    let mut expression = None;
    if let gimli::AttributeValue::Exprloc(expr) = attr {
        expression = Some(expr);
    } else if let Ok(Some(offset)) = dwarf.attr_locations_offset(unit, attr) {
        if let Ok(mut locs) = dwarf.locations(unit, offset) {
            while let Ok(Some(loc)) = locs.next() {
                if pc >= loc.range.begin && pc < loc.range.end {
                    expression = Some(loc.data);
                    break;
                }
            }
        }
    }

    let Some(expr) = expression else {
        return ("".to_string(), "optimized_out".to_string());
    };

    let mut eval = expr.evaluation(unit.encoding());
    let mut result = match eval.evaluate() {
        Ok(res) => res,
        Err(_) => return ("".to_string(), "error".to_string()),
    };

    loop {
        match result {
            gimli::EvaluationResult::Complete => break,
            gimli::EvaluationResult::RequiresRegister { register, .. } => {
                let Some(reg_name) = get_register_name(arch, register) else {
                    return ("".to_string(), "error".to_string());
                };
                let Some(reg_val) = frame.registers.as_ref().and_then(|r| r.get(reg_name)) else {
                    return ("".to_string(), "optimized_out".to_string());
                };
                result = match eval.resume_with_register(gimli::Value::Generic(reg_val.0)) {
                    Ok(res) => res,
                    Err(_) => return ("".to_string(), "error".to_string()),
                };
            }
            gimli::EvaluationResult::RequiresMemory { address, size, .. } => {
                let Some(bytes) = memory.read(address, size as usize) else {
                    return ("".to_string(), "optimized_out".to_string());
                };
                let val = decode_uint(&bytes, endian);
                result = match eval.resume_with_memory(gimli::Value::Generic(val)) {
                    Ok(res) => res,
                    Err(_) => return ("".to_string(), "error".to_string()),
                };
            }
            _ => return ("".to_string(), "optimized_out".to_string()),
        }
    }

    let pieces = eval.result();
    if pieces.is_empty() {
        return ("".to_string(), "optimized_out".to_string());
    }

    match pieces[0].location {
        gimli::Location::Register { register } => {
            let Some(reg_name) = get_register_name(arch, register) else {
                return ("".to_string(), "error".to_string());
            };
            let Some(reg_val) = frame.registers.as_ref().and_then(|r| r.get(reg_name)) else {
                return ("".to_string(), "optimized_out".to_string());
            };
            (reg_val.0.to_string(), "register".to_string())
        }
        gimli::Location::Address { address } => (address.to_string(), "stack".to_string()),
        gimli::Location::Value { value } => {
            let val = match value {
                gimli::Value::U8(v) => v as u64,
                gimli::Value::U16(v) => v as u64,
                gimli::Value::U32(v) => v as u64,
                gimli::Value::U64(v) => v,
                gimli::Value::I8(v) => v as i64 as u64,
                gimli::Value::I16(v) => v as i64 as u64,
                gimli::Value::I32(v) => v as i64 as u64,
                gimli::Value::I64(v) => v as u64,
                _ => 0,
            };
            (val.to_string(), "value".to_string())
        }
        _ => ("".to_string(), "optimized_out".to_string()),
    }
}

/// Resolves the DIE that `die`'s `DW_AT_type` points to, following `DW_AT_abstract_origin`/
/// `DW_AT_specification` (via [`resolve_origin`]) when `die` itself has no `DW_AT_type`.
///
/// Mirrors [`get_type_name_impl`]'s origin-following: the concrete DIE for an inlined
/// parameter/variable commonly carries only `DW_AT_location` (call-site-specific) plus a
/// reference to the abstract DIE that actually has `DW_AT_type`. Without this,
/// [`format_piece_value`] would resolve the correct `type_name` *string* (since
/// `evaluate_variable` calls `get_type_name`, which already follows the origin) but then fail
/// to find any type DIE for its own formatting decision, silently falling back to raw
/// hex/decimal values for every inlined variable regardless of its actual type.
fn resolve_variable_type_die<'u>(
    unit: &'u Unit,
    die: &gimli::DebuggingInformationEntry<R>,
) -> Option<gimli::DebuggingInformationEntry<'u, 'u, R>> {
    let mut current = die.clone();
    for _ in 0..MAX_TYPE_FORMATTING_DEPTH {
        if let Some(gimli::AttributeValue::UnitRef(offset)) = current.attr_value(gimli::DW_AT_type).ok().flatten() {
            return unit.entry(offset).ok();
        }
        current = resolve_origin(unit, &current)?;
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn format_piece_value(
    dwarf: &Dwarf,
    unit: &Unit,
    die: &gimli::DebuggingInformationEntry<R>,
    raw_val: u64,
    is_address: bool,
    memory: MemoryReader,
    arch: object::Architecture,
    endian: gimli::RunTimeEndian,
    depth: u32,
) -> String {
    if depth >= MAX_TYPE_FORMATTING_DEPTH {
        return "<max_depth_exceeded>".to_string();
    }

    let mut resolved_die = resolve_variable_type_die(unit, die);
    for _ in 0..MAX_TYPE_FORMATTING_DEPTH {
        let Some(ref die) = resolved_die else { break };
        if matches!(
            die.tag(),
            gimli::DW_TAG_typedef | gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type | gimli::DW_TAG_reference_type
        ) {
            if let Some(gimli::AttributeValue::UnitRef(offset)) = die.attr_value(gimli::DW_AT_type).ok().flatten() {
                resolved_die = unit.entry(offset).ok();
            } else {
                break;
            }
        } else {
            break;
        }
    }

    let Some(ref t_die) = resolved_die else {
        return if is_address { format!("{raw_val:#x}") } else { raw_val.to_string() };
    };

    let type_name = get_die_name(dwarf, unit, t_die).unwrap_or_default();

    match t_die.tag() {
        gimli::DW_TAG_base_type => {
            let encoding = t_die
                .attr_value(gimli::DW_AT_encoding)
                .ok()
                .flatten()
                .and_then(|attr| match attr {
                    gimli::AttributeValue::Encoding(enc) => Some(enc),
                    _ => None,
                });
            let size = t_die
                .attr_value(gimli::DW_AT_byte_size)
                .ok()
                .flatten()
                .and_then(|attr| attr.udata_value())
                .unwrap_or(4);

            let val = if is_address {
                match memory.read(raw_val, size as usize) {
                    Some(bytes) => decode_uint(&bytes, endian),
                    None => return "<unmapped_address>".to_string(),
                }
            } else {
                raw_val
            };

            if type_name == "bool" || type_name == "_Bool" {
                if val == 0 { "false".to_string() } else { "true".to_string() }
            } else if type_name.contains("char") {
                if size == 1 {
                    format!("'{}'", val as u8 as char)
                } else {
                    val.to_string()
                }
            } else if type_name.contains("float") || type_name.contains("double") {
                match size {
                    4 => format!("{}", f32::from_bits(val as u32)),
                    8 => format!("{}", f64::from_bits(val)),
                    _ => val.to_string(),
                }
            } else if encoding == Some(gimli::DW_ATE_signed) {
                let sval = match size {
                    1 => val as i8 as i64,
                    2 => val as i16 as i64,
                    4 => val as i32 as i64,
                    _ => val as i64,
                };
                sval.to_string()
            } else {
                val.to_string()
            }
        }
        gimli::DW_TAG_pointer_type => {
            let ptr_val = if is_address {
                let ptr_size = match arch {
                    object::Architecture::I386 | object::Architecture::Arm => 4,
                    _ => 8,
                };
                match memory.read(raw_val, ptr_size) {
                    Some(bytes) => decode_uint(&bytes, endian),
                    None => return "<unmapped_address>".to_string(),
                }
            } else {
                raw_val
            };

            if ptr_val == 0 {
                return "0x0".to_string();
            }

            let inner_type_die = t_die
                .attr_value(gimli::DW_AT_type)
                .ok()
                .flatten()
                .and_then(|attr| match attr {
                    gimli::AttributeValue::UnitRef(offset) => Some(offset),
                    _ => None,
                })
                .and_then(|offset| unit.entry(offset).ok());

            if let Some(ref inner_t) = inner_type_die {
                let inner_name = get_die_name(dwarf, unit, inner_t).unwrap_or_default();
                if inner_name == "char" || inner_name == "const char" {
                    if let Some(bytes) = memory.read(ptr_val, 256) {
                        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                        if let Ok(s) = std::str::from_utf8(&bytes[..len]) {
                            return format!("{ptr_val:#x} \"{s}\"");
                        }
                    }
                }
            }

            format!("{ptr_val:#x}")
        }
        gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type => {
            if !is_address {
                return format!("struct {type_name}");
            }

            let mut members = vec![];
            if let Ok(mut tree) = unit.entries_at_offset(t_die.offset()) {
                // Consume the struct DIE itself before walking its members.
                if tree.next_dfs().is_ok() {
                    let mut abs_depth: i64 = 0;
                    while let Ok(Some((delta, entry))) = tree.next_dfs() {
                        abs_depth += delta as i64;
                        if abs_depth <= 0 {
                            break; // left the struct's subtree
                        }
                        if abs_depth > 1 {
                            continue; // nested within a member (e.g. an inner type), not a direct member
                        }
                        if entry.tag() == gimli::DW_TAG_member {
                            let m_name = get_die_name(dwarf, unit, entry).unwrap_or_default();
                            let offset = entry
                                .attr_value(gimli::DW_AT_data_member_location)
                                .ok()
                                .flatten()
                                .and_then(|attr| attr.udata_value())
                                .unwrap_or(0);
                            let m_val = format_piece_value(dwarf, unit, entry, raw_val + offset, true, memory, arch, endian, depth + 1);
                            members.push(format!("{m_name}: {m_val}"));
                        }
                    }
                }
            }

            if members.is_empty() {
                format!("struct {type_name}")
            } else {
                format!("struct {type_name} {{ {} }}", members.join(", "))
            }
        }
        _ => {
            if is_address {
                format!("{raw_val:#x}")
            } else {
                raw_val.to_string()
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_location(
    dwarf: &Dwarf,
    unit: &Unit,
    attr: AttrValue,
    pc: u64,
    frame: &RawFrame,
    memory: MemoryReader,
    arch: object::Architecture,
    endian: gimli::RunTimeEndian,
    die: &gimli::DebuggingInformationEntry<R>,
    frame_base_attr: Option<AttrValue>,
) -> (String, String) {
    let mut expression = None;
    if let gimli::AttributeValue::Exprloc(expr) = attr {
        expression = Some(expr);
    } else if let Ok(Some(offset)) = dwarf.attr_locations_offset(unit, attr) {
        if let Ok(mut locs) = dwarf.locations(unit, offset) {
            while let Ok(Some(loc)) = locs.next() {
                if pc >= loc.range.begin && pc < loc.range.end {
                    expression = Some(loc.data);
                    break;
                }
            }
        }
    }

    let Some(expr) = expression else {
        return ("".to_string(), "optimized_out".to_string());
    };

    let mut eval = expr.evaluation(unit.encoding());
    let mut result = match eval.evaluate() {
        Ok(res) => res,
        Err(_) => return ("".to_string(), "error".to_string()),
    };

    loop {
        match result {
            gimli::EvaluationResult::Complete => break,
            gimli::EvaluationResult::RequiresRegister { register, .. } => {
                let Some(reg_name) = get_register_name(arch, register) else {
                    return ("".to_string(), "error".to_string());
                };
                let Some(reg_val) = frame.registers.as_ref().and_then(|r| r.get(reg_name)) else {
                    return ("".to_string(), "optimized_out".to_string());
                };
                result = match eval.resume_with_register(gimli::Value::Generic(reg_val.0)) {
                    Ok(res) => res,
                    Err(_) => return ("".to_string(), "error".to_string()),
                };
            }
            gimli::EvaluationResult::RequiresFrameBase => {
                let Some(fb_attr) = frame_base_attr.clone() else {
                    return ("".to_string(), "error".to_string());
                };
                let (fb_val, fb_status) = evaluate_frame_base(dwarf, unit, fb_attr, pc, frame, memory, arch, endian);
                if fb_status != "register" && fb_status != "stack" {
                    return ("".to_string(), fb_status);
                }
                let parsed_val: u64 = fb_val.parse().unwrap_or(0);
                result = match eval.resume_with_frame_base(parsed_val) {
                    Ok(res) => res,
                    Err(_) => return ("".to_string(), "error".to_string()),
                };
            }
            gimli::EvaluationResult::RequiresMemory { address, size, .. } => {
                let Some(bytes) = memory.read(address, size as usize) else {
                    return ("".to_string(), "optimized_out".to_string());
                };
                let val = decode_uint(&bytes, endian);
                result = match eval.resume_with_memory(gimli::Value::Generic(val)) {
                    Ok(res) => res,
                    Err(_) => return ("".to_string(), "error".to_string()),
                };
            }
            _ => return ("".to_string(), "optimized_out".to_string()),
        }
    }

    let pieces = eval.result();
    if pieces.is_empty() {
        return ("".to_string(), "optimized_out".to_string());
    }

    match pieces[0].location {
        gimli::Location::Register { register } => {
            let Some(reg_name) = get_register_name(arch, register) else {
                return ("".to_string(), "error".to_string());
            };
            let Some(reg_val) = frame.registers.as_ref().and_then(|r| r.get(reg_name)) else {
                return ("".to_string(), "optimized_out".to_string());
            };
            let formatted = format_piece_value(dwarf, unit, die, reg_val.0, false, memory, arch, endian, 0);
            (formatted, "register".to_string())
        }
        gimli::Location::Address { address } => {
            let formatted = format_piece_value(dwarf, unit, die, address, true, memory, arch, endian, 0);
            (formatted, "stack".to_string())
        }
        gimli::Location::Value { value } => {
            let val = match value {
                gimli::Value::U8(v) => v as u64,
                gimli::Value::U16(v) => v as u64,
                gimli::Value::U32(v) => v as u64,
                gimli::Value::U64(v) => v,
                gimli::Value::I8(v) => v as i64 as u64,
                gimli::Value::I16(v) => v as i64 as u64,
                gimli::Value::I32(v) => v as i64 as u64,
                gimli::Value::I64(v) => v as u64,
                _ => 0,
            };
            let formatted = format_piece_value(dwarf, unit, die, val, false, memory, arch, endian, 0);
            (formatted, "value".to_string())
        }
        _ => ("".to_string(), "optimized_out".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_variable(
    dwarf: &Dwarf,
    unit: &Unit,
    entry: &gimli::DebuggingInformationEntry<R>,
    pc: u64,
    frame: &RawFrame,
    memory: MemoryReader,
    arch: object::Architecture,
    endian: gimli::RunTimeEndian,
    frame_base_attr: Option<AttrValue>,
) -> Option<Variable> {
    let name = get_die_name(dwarf, unit, entry)?;
    let type_name = Some(get_type_name(dwarf, unit, entry));

    // The location is call-site-specific, so prefer the concrete (possibly inlined) DIE's own
    // DW_AT_location; only fall back to the abstract origin's if the concrete DIE has none.
    let location_attr = entry.attr_value(gimli::DW_AT_location).ok().flatten().or_else(|| {
        resolve_origin(unit, entry).and_then(|origin| origin.attr_value(gimli::DW_AT_location).ok().flatten())
    });
    if let Some(attr) = location_attr {
        let (value, status) = evaluate_location(dwarf, unit, attr, pc, frame, memory, arch, endian, entry, frame_base_attr);
        Some(Variable {
            name,
            type_name,
            value: Some(value),
            location_status: Some(status),
        })
    } else {
        Some(Variable {
            name,
            type_name,
            value: None,
            location_status: Some("optimized_out".to_string()),
        })
    }
}

/// Collects the formal parameters and local variables that are in scope for `pc` within
/// `scope_die` (a `DW_TAG_subprogram` or `DW_TAG_inlined_subroutine`).
///
/// Walks the DIE tree using `next_dfs()`'s *delta* depth accumulated into an absolute depth
/// (per gimli's own documented pattern), so that it correctly detects when it has left
/// `scope_die`'s subtree and correctly tracks which nested `DW_TAG_lexical_block`,
/// `DW_TAG_inlined_subroutine`, or `DW_TAG_subprogram` scopes are active (i.e. their PC range
/// contains `pc`) at any given point in the traversal. Without PC-gating the latter two, a
/// scope that structurally contains multiple sibling inlined call sites (or nested local
/// functions/lambdas) would leak variables from whichever ones don't actually cover `pc`.
#[allow(clippy::too_many_arguments)]
fn collect_variables_in_scope(
    dwarf: &Dwarf,
    unit: &Unit,
    scope_die: &gimli::DebuggingInformationEntry<R>,
    pc: u64,
    frame: &RawFrame,
    memory: MemoryReader,
    arch: object::Architecture,
    endian: gimli::RunTimeEndian,
    frame_base_attr: Option<AttrValue>,
) -> (Vec<Variable>, Vec<Variable>) {
    let mut arguments = vec![];
    let mut local_variables = vec![];

    let Ok(mut tree) = unit.entries_at_offset(scope_die.offset()) else {
        return (arguments, local_variables);
    };
    // Consume scope_die itself (always delta 0 on the first call).
    if tree.next_dfs().is_err() {
        return (arguments, local_variables);
    }

    // active_by_depth[d] records whether the lexical block owning depth `d` is active.
    // Index 0 corresponds to scope_die itself, which is always active.
    let mut active_by_depth: Vec<bool> = vec![true];
    let mut abs_depth: i64 = 0;

    while let Ok(Some((delta, entry))) = tree.next_dfs() {
        abs_depth += delta as i64;
        if abs_depth <= 0 {
            break; // left scope_die's subtree
        }
        let depth = abs_depth as usize;

        active_by_depth.truncate(depth);
        let parent_active = *active_by_depth.last().unwrap_or(&true);

        // scope_die itself (index 0) is always active regardless of tag; any occurrence of these
        // tags encountered *during* the walk is necessarily a nested scope and must be
        // PC-gated.
        let is_active = if matches!(
            entry.tag(),
            gimli::DW_TAG_lexical_block | gimli::DW_TAG_inlined_subroutine | gimli::DW_TAG_subprogram
        ) {
            parent_active && die_contains_addr(dwarf, unit, entry, pc)
        } else {
            parent_active
        };
        active_by_depth.push(is_active);

        if !is_active {
            continue;
        }

        match entry.tag() {
            gimli::DW_TAG_formal_parameter => {
                if let Some(var) = evaluate_variable(dwarf, unit, entry, pc, frame, memory, arch, endian, frame_base_attr.clone()) {
                    arguments.push(var);
                }
            }
            gimli::DW_TAG_variable => {
                if let Some(var) = evaluate_variable(dwarf, unit, entry, pc, frame, memory, arch, endian, frame_base_attr.clone()) {
                    local_variables.push(var);
                }
            }
            _ => {}
        }
    }

    (arguments, local_variables)
}

/// An indexed `DW_TAG_subprogram`/`DW_TAG_inlined_subroutine` PC range within a module.
#[derive(Debug)]
struct FunctionRange {
    low_pc: u64,
    high_pc: u64,
    /// Running maximum of `high_pc` over all entries at or before this one in `ModuleDwarf::ranges`
    /// (which is sorted by `low_pc`). Lets a lookup stop scanning backward as soon as no earlier
    /// entry could possibly still contain the address — see `ModuleDwarf::find_range`.
    max_high_pc_prefix: u64,
    unit_index: usize,
    die_offset: gimli::UnitOffset<usize>,
    /// The nearest enclosing `DW_TAG_subprogram`'s `DW_AT_frame_base`, precomputed once here
    /// instead of being re-derived by re-scanning the unit for every variable lookup.
    frame_base_attr: Option<AttrValue>,
}

/// The parsed DWARF debug info for a module. See [`ModuleDwarfCache`] for its caching lifetime.
#[derive(Debug)]
pub(crate) struct ModuleDwarf {
    dwarf: Dwarf,
    arch: object::Architecture,
    endian: gimli::RunTimeEndian,
    units: Vec<Unit>,
    /// All function PC ranges in the module, sorted by `low_pc`. See `find_range`.
    ranges: Vec<FunctionRange>,
    /// Approximate in-memory size (uncompressed DWARF section bytes), used to weight entries in
    /// [`ModuleDwarfCache`] so it's bounded by actual memory footprint, not just item count —
    /// a single large library's debug info can be 100+ MiB.
    total_bytes: usize,
}

impl ModuleDwarf {
    /// Finds every indexed range containing `addr` -- i.e. the full DWARF inline-scope chain
    /// at that address -- ordered from innermost (smallest) to outermost (largest).
    ///
    /// Ranges strictly nest for well-formed DWARF (a `DW_TAG_inlined_subroutine`'s PC range is
    /// always a subset of its enclosing scope's), so sorting all matches by size reconstructs
    /// the same innermost-to-outermost chain SymCache's own inline expansion produces: one
    /// entry per depth, from the innermost inlined call site out to the top-level
    /// `DW_TAG_subprogram`. See [`VariableExtractor::extract_for_frame`], which zips this
    /// against the group of `SymbolicatedFrame`s a single physical frame expanded into.
    ///
    /// Ranges are sorted by `low_pc`; candidates are all entries at or before the first one
    /// whose `low_pc` exceeds `addr`. Scanning backward from there stops as soon as
    /// `max_high_pc_prefix <= addr`, since at that point no earlier entry (even one with a
    /// smaller `low_pc`) can have a `high_pc` large enough to contain `addr` either. For
    /// properly nested DWARF this terminates after a handful of steps (bounded by inlining
    /// depth), turning what used to be an O(all DIEs in the unit) scan per frame into an
    /// O(log n + nesting depth) lookup.
    fn find_range_chain(&self, addr: u64) -> Vec<&FunctionRange> {
        let end = self.ranges.partition_point(|r| r.low_pc <= addr);
        let mut matches: Vec<&FunctionRange> = Vec::new();
        for r in self.ranges[..end].iter().rev() {
            if r.max_high_pc_prefix <= addr {
                break;
            }
            if addr < r.high_pc {
                matches.push(r);
            }
        }
        matches.sort_by_key(|r| r.high_pc - r.low_pc);
        matches
    }
}

/// Advances a per-depth "current enclosing scope value" tracker by one DFS step and returns the
/// value in effect for the DIE just visited.
///
/// `by_depth[d]` holds the value in effect for a DIE at absolute depth `d`: `own` if `is_scope`
/// (this DIE defines a new scope, e.g. `DW_TAG_subprogram`), otherwise inherited from the
/// nearest enclosing scope. Truncating to `depth` before reading/pushing -- rather than just
/// pushing -- is what makes leaving a nested scope's subtree correctly restore the enclosing
/// scope's value for later siblings, instead of leaking the nested one to them.
///
/// Generic over `T` so this can be unit-tested with plain markers instead of real DWARF
/// attribute values: a compiled fixture can't reliably demonstrate a bug here, since e.g. GCC's
/// default `DW_OP_call_frame_cfa` frame_base is byte-identical across every function regardless
/// of nesting, which would make an end-to-end test pass even with the restoration missing.
fn track_by_depth<T: Clone>(by_depth: &mut Vec<Option<T>>, depth: usize, is_scope: bool, own: Option<T>) -> Option<T> {
    by_depth.truncate(depth);
    let inherited = by_depth.last().cloned().flatten();
    let current = if is_scope { own } else { inherited };
    by_depth.push(current.clone());
    current
}

/// Parses an object file's DWARF sections into an owned, cacheable `Dwarf` value, and builds
/// the function-range index used by `ModuleDwarf::find_range`.
fn load_module_dwarf(data: &[u8]) -> Option<ModuleDwarf> {
    use object::ObjectSection;

    let file = object::File::parse(data).ok()?;
    let endian = if file.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    };
    let arch = file.architecture();

    let mut total_bytes = 0usize;
    let dwarf: Dwarf = gimli::Dwarf::load(|id| -> Result<R, ()> {
        let bytes: Arc<[u8]> = match file.section_by_name(id.name()) {
            Some(section) => Arc::from(section.uncompressed_data().map_err(|_| ())?.into_owned()),
            None => Arc::from(Vec::new()),
        };
        total_bytes += bytes.len();
        Ok(gimli::EndianArcSlice::new(bytes, endian))
    })
    .ok()?;

    let mut units = Vec::new();
    let mut ranges = Vec::new();

    let mut header_iter = dwarf.units();
    while let Ok(Some(header)) = header_iter.next() {
        let Ok(unit) = dwarf.unit(header) else {
            continue;
        };
        let unit_index = units.len();

        // frame_base_by_depth[d] holds the DW_AT_frame_base in effect for a DIE at absolute
        // depth `d`: either that DIE's own value if it's a DW_TAG_subprogram, or its parent's
        // (inherited) value otherwise. Restoring it via `track_by_depth` (mirroring
        // `collect_variables_in_scope`'s `active_by_depth`) as the DFS walk leaves a nested
        // DW_TAG_subprogram's subtree (a local function or lambda emitted as its own
        // subprogram DIE) is what keeps the outer function's frame base for later siblings,
        // instead of leaking the inner one to them.
        let mut frame_base_by_depth: Vec<Option<AttrValue>> = Vec::new();
        let mut abs_depth: i64 = 0;
        let mut entries = unit.entries();
        while let Ok(Some((delta, entry))) = entries.next_dfs() {
            abs_depth += delta as i64;
            if abs_depth < 0 {
                break;
            }
            let depth = abs_depth as usize;
            let is_subprogram = entry.tag() == gimli::DW_TAG_subprogram;
            let own_frame_base = if is_subprogram {
                entry.attr_value(gimli::DW_AT_frame_base).ok().flatten()
            } else {
                None
            };
            let current_subprogram_frame_base =
                track_by_depth(&mut frame_base_by_depth, depth, is_subprogram, own_frame_base);

            if entry.tag() == gimli::DW_TAG_subprogram || entry.tag() == gimli::DW_TAG_inlined_subroutine {
                for (low, high) in die_ranges(&dwarf, &unit, entry) {
                    if high > low {
                        ranges.push(FunctionRange {
                            low_pc: low,
                            high_pc: high,
                            max_high_pc_prefix: 0, // filled in below, after sorting
                            unit_index,
                            die_offset: entry.offset(),
                            frame_base_attr: current_subprogram_frame_base.clone(),
                        });
                    }
                }
            }
        }

        units.push(unit);
    }

    ranges.sort_by_key(|r| r.low_pc);
    let mut running_max = 0u64;
    for r in &mut ranges {
        running_max = running_max.max(r.high_pc);
        r.max_high_pc_prefix = running_max;
    }

    Some(ModuleDwarf {
        dwarf,
        arch,
        endian,
        units,
        ranges,
        total_bytes,
    })
}

/// Parsed, indexed DWARF debug info, cached across *requests* and keyed by the module's
/// `DebugId`.
///
/// The raw object bytes are already disk-cached across requests by the existing object-fetch
/// infrastructure, but parsing them into a `Dwarf`/`Unit`s and building the function-range index
/// (see `ModuleDwarf::find_range`) is CPU-bound work that isn't — for a single large library
/// (100+ MiB of `.debug_info`, common for heavily-templated C++) this can take several seconds.
/// Since many crashes commonly land in the same handful of popular system libraries, redoing
/// that work on every request would make this feature a real bottleneck at volume. Entries are
/// weighted by `ModuleDwarf::total_bytes` so the cache is bounded by actual memory footprint.
pub type ModuleDwarfCache = moka::sync::Cache<symbolic::common::DebugId, Option<Arc<ModuleDwarf>>>;

pub fn new_module_dwarf_cache() -> ModuleDwarfCache {
    ModuleDwarfCache::builder()
        .max_capacity(512 * 1024 * 1024) // 512 MiB
        .weigher(|_, v: &Option<Arc<ModuleDwarf>>| {
            v.as_ref().map_or(1, |m| m.total_bytes.try_into().unwrap_or(u32::MAX))
        })
        .build()
}

/// Extracts function arguments and local variables from DWARF debug info for the frames of a
/// symbolication request, evaluating their locations against the minidump's registers and
/// stack memory.
///
/// Parsed DWARF is cached across requests in `dwarf_cache` (see [`ModuleDwarfCache`]), since a
/// single stacktrace commonly has many frames in the same binary and many requests commonly
/// share the same handful of popular libraries.
pub struct VariableExtractor<'a, 'md> {
    memory_list: Option<minidump::MinidumpMemoryList<'md>>,
    memory64_list: Option<minidump::MinidumpMemory64List<'md>>,
    dwarf_cache: &'a ModuleDwarfCache,
}

impl<'a, 'md> VariableExtractor<'a, 'md> {
    pub fn new(minidump: &'md minidump::Minidump<'md, ByteView<'md>>, dwarf_cache: &'a ModuleDwarfCache) -> Self {
        Self {
            memory_list: minidump.get_stream::<minidump::MinidumpMemoryList>().ok(),
            memory64_list: minidump.get_stream::<minidump::MinidumpMemory64List>().ok(),
            dwarf_cache,
        }
    }

    /// Populates `frame.arguments` and `frame.local_variables` in place, if DWARF debug info
    /// and a matching scope can be found for the frame's address.
    ///
    /// `depth` is this frame's position (0 = innermost) within the group of
    /// `SymbolicatedFrame`s that a single physical stack frame expanded into via inlining --
    /// all of which share the same `instruction_addr`. Callers must pass the right depth for
    /// each (see the caller in `symbolicate.rs`, which groups consecutive frames by
    /// `original_index`); passing 0 unconditionally would give every inlined frame the
    /// innermost scope's variables instead of the one matching its own depth.
    pub fn extract_for_frame(&mut self, frame: &mut RawFrame, module_lookup: &ModuleLookup, depth: usize) {
        let Some(lookup_result) = module_lookup.lookup_cache(frame.instruction_addr.0, frame.addr_mode) else {
            return;
        };
        let Some(relative_addr) = lookup_result.relative_addr else {
            return;
        };
        let module_index = lookup_result.module_index;

        let Some(handle) = module_lookup.get_debug_object(module_index) else {
            return;
        };
        let debug_id = handle.object().debug_id();
        let module_dwarf = self
            .dwarf_cache
            .get_with(debug_id, || load_module_dwarf(handle.data()).map(Arc::new));
        let Some(module_dwarf) = module_dwarf else {
            return;
        };

        let ranges = module_dwarf.find_range_chain(relative_addr);
        // Clamp to the outermost known range rather than bailing if `depth` runs past the
        // DWARF-inferred chain (e.g. SymCache found deeper inline info than our own DWARF
        // walk did) -- degrading to the closest available scope beats extracting nothing.
        let Some(&range) = ranges.get(depth).or_else(|| ranges.last()) else {
            return;
        };
        let unit = &module_dwarf.units[range.unit_index];
        let Ok(entry) = unit.entry(range.die_offset) else {
            return;
        };

        let memory = MemoryReader {
            memory_list: self.memory_list.as_ref(),
            memory64_list: self.memory64_list.as_ref(),
        };

        let (args, locals) = collect_variables_in_scope(
            &module_dwarf.dwarf,
            unit,
            &entry,
            relative_addr,
            frame,
            memory,
            module_dwarf.arch,
            module_dwarf.endian,
            range.frame_base_attr.clone(),
        );
        frame.arguments = args;
        frame.local_variables = locals;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Compiles a tiny C fixture with `cc -g -O0` and returns the resulting executable's bytes.
    ///
    /// `add` has two sibling `DW_TAG_formal_parameter`s followed by a sibling
    /// `DW_TAG_variable` and a nested `DW_TAG_lexical_block` — exactly the DIE shape that
    /// unconditionally panicked (subtract-with-overflow / unbounded `Vec` growth) before
    /// `collect_variables_in_scope` was fixed to track absolute depth instead of gimli's
    /// per-step delta depth.
    fn compile_fixture() -> Vec<u8> {
        let dir = std::env::temp_dir();
        let src_path = dir.join("symbolicator_variables_test_fixture.c");
        let bin_path = dir.join("symbolicator_variables_test_fixture_bin");
        std::fs::write(
            &src_path,
            r#"
int add(int a, int b) {
    int sum = a + b;
    if (sum > 0) {
        int doubled = sum * 2;
        return doubled;
    }
    return sum;
}

int main(void) {
    return add(3, 4);
}
"#,
        )
        .expect("failed to write fixture source");

        let status = Command::new("cc")
            .arg("-g")
            .arg("-O0")
            .arg("-o")
            .arg(&bin_path)
            .arg(&src_path)
            .status()
            .expect("failed to invoke cc; a C compiler is required to run this test");
        assert!(status.success(), "cc failed to compile the test fixture");

        std::fs::read(&bin_path).expect("failed to read compiled fixture")
    }

    /// Compiles a fixture whose `inc` is force-inlined into `caller` even at `-O0`, so it
    /// reliably produces a real `DW_TAG_inlined_subroutine` -- unlike ordinary calls, which
    /// aren't inlined at `-O0` and so never emit one, meaning `collect_variables_in_scope`'s
    /// scope-gating logic and `find_range_chain`'s scope-nesting logic would otherwise go
    /// completely untested against real (rather than hand-assembled) DWARF.
    ///
    /// Confirmed with `readelf --debug-dump=info` on this fixture that GCC emits `inc`'s
    /// `DW_TAG_formal_parameter x` with only `DW_AT_abstract_origin` + `DW_AT_location`, no
    /// `DW_AT_type` of its own -- the shape `format_piece_value` needs to follow the origin for.
    fn compile_inline_fixture() -> Vec<u8> {
        let dir = std::env::temp_dir();
        let src_path = dir.join("symbolicator_variables_test_inline_fixture.c");
        let bin_path = dir.join("symbolicator_variables_test_inline_fixture_bin");
        std::fs::write(
            &src_path,
            r#"
static inline __attribute__((always_inline)) int inc(int x) {
    return x + 1;
}

int caller(int a) {
    int r = inc(a);
    return r;
}

int main(void) {
    return caller(1);
}
"#,
        )
        .expect("failed to write inline fixture source");

        let status = Command::new("cc")
            .arg("-g")
            .arg("-O0")
            .arg("-o")
            .arg(&bin_path)
            .arg(&src_path)
            .status()
            .expect("failed to invoke cc; a C compiler is required to run this test");
        assert!(status.success(), "cc failed to compile the inline test fixture");

        std::fs::read(&bin_path).expect("failed to read compiled inline fixture")
    }

    #[test]
    fn collects_sibling_arguments_and_nested_locals_without_panicking() {
        let bytes = compile_fixture();
        let module = load_module_dwarf(&bytes).expect("failed to parse DWARF from fixture");
        let dwarf = &module.dwarf;
        let memory = MemoryReader {
            memory_list: None,
            memory64_list: None,
        };
        let frame = RawFrame::default();

        let mut found = false;
        let mut units = dwarf.units();
        while let Ok(Some(header)) = units.next() {
            let unit = dwarf.unit(header).expect("failed to parse unit");
            let mut entries = unit.entries();
            while let Ok(Some((_, entry))) = entries.next_dfs() {
                if entry.tag() != gimli::DW_TAG_subprogram {
                    continue;
                }
                if get_die_name(dwarf, &unit, entry).as_deref() != Some("add") {
                    continue;
                }
                found = true;

                let frame_base = entry.attr_value(gimli::DW_AT_frame_base).ok().flatten();
                let low_pc = get_low_pc(entry).expect("add should have a low_pc");

                // Find the nested lexical block's own low_pc so we can pick a pc that's
                // reliably inside it, rather than guessing based on the function's high_pc
                // (which may include epilogue instructions the compiler placed outside the
                // block's own PC range).
                let mut block_pc = None;
                let mut block_entries = unit.entries_at_offset(entry.offset()).expect("valid offset");
                while let Ok(Some((_, block_entry))) = block_entries.next_dfs() {
                    if block_entry.tag() == gimli::DW_TAG_lexical_block {
                        block_pc = get_low_pc(block_entry);
                        break;
                    }
                }
                let block_pc = block_pc.expect("add should contain a nested lexical block");

                // At the very first instruction, `a` and `b` (siblings) and `sum` are in
                // scope; `doubled`, nested in an inactive lexical block, is not. Evaluating
                // this must not panic even though `b` is a same-depth sibling of `a` (delta
                // depth 0) — the exact shape that used to crash.
                let (args, locals) = collect_variables_in_scope(
                    dwarf, &unit, entry, low_pc, &frame, memory, module.arch, module.endian, frame_base.clone(),
                );
                let mut arg_names: Vec<_> = args.iter().map(|v| v.name.clone()).collect();
                arg_names.sort();
                assert_eq!(arg_names, vec!["a".to_string(), "b".to_string()]);
                let local_names: Vec<_> = locals.iter().map(|v| v.name.clone()).collect();
                assert_eq!(local_names, vec!["sum".to_string()]);

                // Inside the nested lexical block, collecting variables again must also not
                // panic when popping back out of the block on a later call, and `doubled`
                // should now be visible too.
                let (_args, locals) = collect_variables_in_scope(
                    dwarf, &unit, entry, block_pc, &frame, memory, module.arch, module.endian, frame_base,
                );
                let mut local_names: Vec<_> = locals.iter().map(|v| v.name.clone()).collect();
                local_names.sort();
                assert!(local_names.contains(&"doubled".to_string()));
                assert!(local_names.contains(&"sum".to_string()));
            }
        }

        assert!(found, "did not find DW_TAG_subprogram 'add' in the compiled fixture");
    }

    /// Exercises the actual value-evaluation pipeline (not just DIE scoping/traversal): a
    /// register-resident value via `DW_OP_regN`, and a stack-resident value reached via
    /// `DW_OP_fbreg` + a register-resident frame base + a real memory read. The location
    /// expressions are hand-crafted DWARF bytecode rather than relying on the host compiler's
    /// specific register-allocation/spill decisions, so the test is deterministic and portable
    /// while still driving the real `gimli::Evaluation` state machine
    /// (`RequiresRegister`/`RequiresFrameBase`/`RequiresMemory`) end-to-end.
    #[test]
    fn evaluates_register_and_stack_resident_values() {
        let bytes = compile_fixture();
        let module = load_module_dwarf(&bytes).expect("failed to parse DWARF from fixture");
        let dwarf = &module.dwarf;
        let arch = module.arch;
        let endian = module.endian;
        assert_eq!(arch, object::Architecture::X86_64, "test assumes host is x86_64");

        let pc = 0; // unused by these hand-crafted (non-list) location expressions
        let mut found = false;

        let mut units = dwarf.units();
        while let Ok(Some(header)) = units.next() {
            let unit = dwarf.unit(header).expect("failed to parse unit");
            let mut entries = unit.entries();
            while let Ok(Some((_, entry))) = entries.next_dfs() {
                if entry.tag() != gimli::DW_TAG_formal_parameter || get_die_name(dwarf, &unit, entry).as_deref() != Some("a") {
                    continue;
                }
                found = true;
                // Use `a`'s real `int` DW_AT_type as formatting context; the *location* we
                // test is entirely synthetic hand-crafted bytecode, not derived from wherever
                // the compiler actually decided to put `a`.
                let param_die = entry;

                // DW_OP_reg0: value is directly in DWARF register 0, which is "rax" on x86_64.
                let reg_expr = gimli::Expression(gimli::EndianArcSlice::new(Arc::from(vec![0x50u8]), endian));
                let mut frame = RawFrame::default();
                let mut regs = crate::interface::Registers::default();
                regs.insert("rax".to_string(), symbolicator_service::utils::hex::HexValue(42));
                frame.registers = Some(regs);
                let no_memory = MemoryReader {
                    memory_list: None,
                    memory64_list: None,
                };

                let (value, status) = evaluate_location(
                    dwarf,
                    &unit,
                    gimli::AttributeValue::Exprloc(reg_expr),
                    pc,
                    &frame,
                    no_memory,
                    arch,
                    endian,
                    param_die,
                    None,
                );
                assert_eq!(status, "register");
                assert_eq!(value, "42");

                // DW_OP_fbreg -4: value is at (frame_base - 4). Frame base is DW_OP_reg6
                // ("rbp" on x86_64), which we set to 0x1000, so the effective address is 0xFFC.
                let stack_expr = gimli::Expression(gimli::EndianArcSlice::new(Arc::from(vec![0x91u8, 0x7c]), endian));
                let frame_base_expr =
                    gimli::AttributeValue::Exprloc(gimli::Expression(gimli::EndianArcSlice::new(Arc::from(vec![0x56u8]), endian)));

                let mut frame = RawFrame::default();
                let mut regs = crate::interface::Registers::default();
                regs.insert("rbp".to_string(), symbolicator_service::utils::hex::HexValue(0x1000));
                frame.registers = Some(regs);

                let region = minidump::MinidumpMemory {
                    desc: Default::default(),
                    base_address: 0xFFC,
                    size: 4,
                    bytes: &[42, 0, 0, 0],
                    endian: minidump::Endian::Little,
                };
                let memory_list = minidump::MinidumpMemoryList::from_regions(vec![region]);
                let memory = MemoryReader {
                    memory_list: Some(&memory_list),
                    memory64_list: None,
                };

                let (value, status) = evaluate_location(
                    dwarf,
                    &unit,
                    gimli::AttributeValue::Exprloc(stack_expr),
                    pc,
                    &frame,
                    memory,
                    arch,
                    endian,
                    param_die,
                    Some(frame_base_expr),
                );
                assert_eq!(status, "stack");
                assert_eq!(value, "42");
            }
        }

        assert!(found, "did not find formal_parameter 'a' in the compiled fixture");
    }

    #[test]
    fn aarch64_frame_pointer_register_is_named_fp_not_x29() {
        // Regression test: rust-minidump's `valid_registers()` names AArch64 register 29 "fp"
        // (its canonical frame-pointer register, per `CONTEXT_ARM64::REGISTERS` in
        // rust-minidump's context.rs), not "x29". `frame.registers` is keyed by that canonical
        // name, so returning "x29" here made every lookup for this DWARF register -- used
        // constantly as the frame-base register on AArch64 -- silently miss and report the
        // variable as optimized out.
        assert_eq!(get_register_name(object::Architecture::Aarch64, gimli::Register(29)), Some("fp"));
        assert_eq!(get_register_name(object::Architecture::Aarch64, gimli::Register(30)), Some("lr"));
        assert_eq!(get_register_name(object::Architecture::Aarch64, gimli::Register(28)), Some("x28"));
    }

    #[test]
    fn frame_base_restored_after_leaving_nested_subprogram_subtree() {
        // Simulates a DFS walk: CU root (depth 0) -> outer subprogram (depth 1, frame_base=A)
        // -> nested inner subprogram (depth 2, frame_base=B) -> a sibling of `inner`, still a
        // direct child of `outer` (back to depth 2, no own frame_base).
        //
        // Regression test for the bug where `current_subprogram_frame_base` was overwritten on
        // every DW_TAG_subprogram and never restored: the depth-2 sibling must inherit
        // `outer`'s frame_base (A), not `inner`'s leaked one (B).
        let mut by_depth: Vec<Option<&str>> = Vec::new();
        assert_eq!(track_by_depth(&mut by_depth, 0, false, None), None); // CU root
        assert_eq!(track_by_depth(&mut by_depth, 1, true, Some("A")), Some("A")); // outer
        assert_eq!(track_by_depth(&mut by_depth, 2, true, Some("B")), Some("B")); // inner (nested)
        assert_eq!(track_by_depth(&mut by_depth, 2, false, None), Some("A")); // sibling of inner
    }

    #[test]
    fn find_range_chain_orders_innermost_inline_scope_first() {
        let bytes = compile_inline_fixture();
        let module = load_module_dwarf(&bytes).expect("failed to parse DWARF from inline fixture");

        let mut inline_pc = None;
        for unit in &module.units {
            let mut entries = unit.entries();
            while let Ok(Some((_, entry))) = entries.next_dfs() {
                if entry.tag() == gimli::DW_TAG_inlined_subroutine {
                    inline_pc = get_low_pc(entry);
                }
            }
        }
        let pc = inline_pc.expect("fixture should contain an inlined_subroutine for `inc`");

        let chain = module.find_range_chain(pc);
        assert!(
            chain.len() >= 2,
            "expected at least [inlined_subroutine, subprogram] at an inlined pc, got {chain:?}"
        );

        // Innermost (smallest) first.
        for pair in chain.windows(2) {
            let inner_size = pair[0].high_pc - pair[0].low_pc;
            let outer_size = pair[1].high_pc - pair[1].low_pc;
            assert!(inner_size <= outer_size, "chain not ordered innermost-first: {chain:?}");
        }

        // The innermost entry must actually be the inlined_subroutine, not `caller` itself --
        // this is the exact bug: extraction always landing on the smallest range is correct
        // for depth 0, but every deeper physical frame in the same group needs the outer ones.
        let unit = &module.units[chain[0].unit_index];
        let innermost_die = unit.entry(chain[0].die_offset).expect("valid die offset");
        assert_eq!(innermost_die.tag(), gimli::DW_TAG_inlined_subroutine);

        let outermost = chain.last().unwrap();
        let outer_unit = &module.units[outermost.unit_index];
        let outer_die = outer_unit.entry(outermost.die_offset).expect("valid die offset");
        assert_eq!(outer_die.tag(), gimli::DW_TAG_subprogram);
        assert_eq!(get_die_name(&module.dwarf, outer_unit, &outer_die).as_deref(), Some("caller"));
    }

    #[test]
    fn inlined_parameter_value_follows_abstract_origin_for_type() {
        let bytes = compile_inline_fixture();
        let module = load_module_dwarf(&bytes).expect("failed to parse DWARF from inline fixture");
        let dwarf = &module.dwarf;
        assert_eq!(module.arch, object::Architecture::X86_64, "test assumes host is x86_64");
        let endian = module.endian;

        let mut found = false;
        for unit in &module.units {
            let mut entries = unit.entries();
            while let Ok(Some((_, entry))) = entries.next_dfs() {
                if entry.tag() != gimli::DW_TAG_inlined_subroutine {
                    continue;
                }
                let mut param_tree = unit.entries_at_offset(entry.offset()).expect("valid offset");
                param_tree.next_dfs().expect("consume the inlined_subroutine itself");
                let Ok(Some((_, param))) = param_tree.next_dfs() else {
                    continue;
                };
                if param.tag() != gimli::DW_TAG_formal_parameter {
                    continue;
                }
                // Confirm the fixture still has the shape this test exists to cover, so a
                // future toolchain/flag change that starts emitting DW_AT_type directly on the
                // concrete DIE doesn't leave this test silently exercising nothing.
                assert!(
                    param.attr_value(gimli::DW_AT_type).ok().flatten().is_none(),
                    "fixture assumption changed: inlined formal_parameter now has its own DW_AT_type"
                );
                found = true;

                let low_pc = get_low_pc(entry).expect("inlined_subroutine should have a low_pc");

                // `DW_OP_fbreg 0` (frame_base + 0), with frame_base = DW_OP_reg6 ("rbp") --
                // synthetic locations, independent of whatever real stack offset GCC picked, so
                // we fully control the raw bytes read back. 0xFFFFFFFF only formats as the
                // sensible `int` value -1 if type resolution actually succeeds; the old bug
                // (no DW_AT_type on the concrete DIE, no abstract-origin fallback) instead fell
                // back to an untyped hex dump of the address.
                let stack_expr = gimli::Expression(gimli::EndianArcSlice::new(Arc::from(vec![0x91u8, 0x00]), endian));
                let frame_base_expr = gimli::AttributeValue::Exprloc(gimli::Expression(gimli::EndianArcSlice::new(
                    Arc::from(vec![0x56u8]),
                    endian,
                )));

                let mut frame = RawFrame::default();
                let mut regs = crate::interface::Registers::default();
                regs.insert("rbp".to_string(), symbolicator_service::utils::hex::HexValue(0x2000));
                frame.registers = Some(regs);

                let region = minidump::MinidumpMemory {
                    desc: Default::default(),
                    base_address: 0x2000,
                    size: 4,
                    bytes: &[0xFF, 0xFF, 0xFF, 0xFF],
                    endian: minidump::Endian::Little,
                };
                let memory_list = minidump::MinidumpMemoryList::from_regions(vec![region]);
                let memory = MemoryReader {
                    memory_list: Some(&memory_list),
                    memory64_list: None,
                };

                let (value, status) = evaluate_location(
                    dwarf,
                    unit,
                    gimli::AttributeValue::Exprloc(stack_expr),
                    low_pc,
                    &frame,
                    memory,
                    module.arch,
                    endian,
                    param,
                    Some(frame_base_expr),
                );
                assert_eq!(status, "stack");
                assert_eq!(
                    value, "-1",
                    "inlined parameter's raw 0xFFFFFFFF should format as signed int -1 via the \
                     abstract origin's type, not fall back to an untyped hex dump (got {value:?})"
                );
            }
        }
        assert!(found, "did not find an inlined_subroutine formal_parameter without its own DW_AT_type");
    }
}
