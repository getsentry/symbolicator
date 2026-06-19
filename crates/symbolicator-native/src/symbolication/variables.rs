//! Extraction of local variable **values** from minidump memory + debug info.
//!
//! This module populates [`RawFrame::vars`] with variable names, types, and
//! **runtime values** read from minidump memory using register state and DWARF/PDB
//! location expressions.
//!
//! It also performs memory corruption detection on pointer values (null deref,
//! use-after-free sentinels, uninitialized memory patterns).

use std::collections::HashMap;

use serde_json::{json, Value as JsonValue};
use symbolic::common::Arch;
use symbolic::debuginfo::{Function, ObjectDebugSession, PrimitiveEncoding, VariableLocation, VariableType};

use crate::interface::{CompleteStacktrace, SymbolicatedFrame};

use super::module_lookup::{DebugSessions, ModuleLookup};

// ── Memory snapshot types ───────────────────────────────────────────────────

/// A captured region of process memory from a minidump.
#[derive(Debug, Clone)]
pub struct MemoryRegion {
    pub start_addr: u64,
    pub data: Vec<u8>,
}

/// Snapshot of process memory and register state extracted from a minidump.
///
/// This is extracted during stackwalk (while the minidump is still in memory)
/// and passed through to the variable extraction phase.
#[derive(Debug, Clone)]
pub struct MinidumpMemorySnapshot {
    /// Memory regions from the minidump (stack + heap).
    pub regions: Vec<MemoryRegion>,
    /// Per-thread, per-frame register state.
    /// `thread_frame_registers[thread_idx][frame_idx]` = register name → value.
    pub thread_frame_registers: Vec<Vec<HashMap<String, u64>>>,
    /// CPU architecture (determines register name mapping and pointer size).
    pub arch: Arch,
}

impl MinidumpMemorySnapshot {
    /// Read `size` bytes starting at `addr` from any captured memory region.
    pub fn read_bytes(&self, addr: u64, size: usize) -> Option<Vec<u8>> {
        for region in &self.regions {
            let end = region.start_addr.saturating_add(region.data.len() as u64);
            if addr >= region.start_addr && addr < end {
                let offset = (addr - region.start_addr) as usize;
                if offset + size <= region.data.len() {
                    return Some(region.data[offset..offset + size].to_vec());
                }
            }
        }
        None
    }

    /// Read a u64 from memory (or u32 zero-extended on 32-bit arch).
    #[allow(dead_code)] // Will be used for pointer dereference / string reading
    fn read_pointer(&self, addr: u64) -> Option<u64> {
        let ptr_size = pointer_size(self.arch);
        let bytes = self.read_bytes(addr, ptr_size)?;
        Some(if ptr_size == 4 {
            u32::from_le_bytes(bytes[..4].try_into().ok()?) as u64
        } else {
            u64::from_le_bytes(bytes[..8].try_into().ok()?)
        })
    }
}

// ── Corruption sentinels ────────────────────────────────────────────────────

struct Sentinel {
    /// 32-bit pattern (matched against low 32 bits for 32-bit pointers,
    /// or repeated as 64-bit for 64-bit pointers).
    pattern32: u32,
    kind: &'static str,
    description: &'static str,
}

const SENTINELS: &[Sentinel] = &[
    Sentinel { pattern32: 0xFEEEFEEE, kind: "heap_free", description: "Windows HeapFree sentinel — likely use-after-free" },
    Sentinel { pattern32: 0xCDCDCDCD, kind: "uninitialized_heap", description: "MSVC uninitialized heap memory" },
    Sentinel { pattern32: 0xCCCCCCCC, kind: "uninitialized_stack", description: "MSVC uninitialized stack memory" },
    Sentinel { pattern32: 0xBAADF00D, kind: "local_alloc", description: "Windows LocalAlloc sentinel" },
    Sentinel { pattern32: 0xFDFDFDFD, kind: "heap_guard", description: "MSVC heap guard bytes" },
    Sentinel { pattern32: 0xABABABAB, kind: "heap_guard_after", description: "Windows heap guard after allocation" },
    Sentinel { pattern32: 0xDDDDDDDD, kind: "freed_heap", description: "MSVC freed heap memory" },
    Sentinel { pattern32: 0xDEADBEEF, kind: "dead_marker", description: "Dead marker (0xDEADBEEF)" },
];

/// Check a pointer value for known corruption patterns.
///
/// Checks are ordered by confidence (highest first) with early returns so that
/// the most specific diagnosis wins.
fn check_pointer_annotations(value: u64, memory: &MinidumpMemorySnapshot) -> Vec<JsonValue> {
    let arch = memory.arch;
    let ptr_size = pointer_size(arch);
    let mut annotations = Vec::new();

    // ── 1. Null pointer (exact zero) ────────────────────────────────────────
    if value == 0 {
        annotations.push(json!({"type": "null_pointer"}));
        return annotations;
    }

    // ── 2. Near-null (< 64 KB) ─────────────────────────────────────────────
    // The first 64 KB is unmapped on all supported OSes (Windows reserves it
    // explicitly; Linux has mmap_min_addr). A small non-zero pointer is almost
    // always `NULL->field_at_offset`.
    if value < 0x10000 {
        annotations.push(json!({
            "type": "null_pointer",
            "description": format!(
                "Near-null address (0x{value:x}) — likely null pointer with struct field offset"
            ),
        }));
        return annotations;
    }

    // ── 3. Known sentinel values (exact match) ─────────────────────────────
    for sentinel in SENTINELS {
        let matches = if ptr_size == 4 {
            value as u32 == sentinel.pattern32
        } else {
            // MSVC fills in 32-bit chunks, so a 64-bit pointer gets the
            // pattern repeated in both halves.
            let repeated = (sentinel.pattern32 as u64) << 32 | sentinel.pattern32 as u64;
            value == repeated
        };
        if matches {
            annotations.push(json!({
                "type": "sentinel_value",
                "pattern": sentinel.kind,
                "description": sentinel.description,
            }));
            return annotations;
        }
    }

    // ── 4. Non-canonical address (AMD64 only) ──────────────────────────────
    // Bits 48–63 must be copies of bit 47. Any other value is physically
    // impossible and means the pointer was corrupted.
    if arch == Arch::Amd64 {
        let bit47 = (value >> 47) & 1;
        let high_bits = value >> 48;
        if (bit47 == 0 && high_bits != 0) || (bit47 == 1 && high_bits != 0xFFFF) {
            annotations.push(json!({
                "type": "non_canonical_address",
                "description": "Non-canonical x86_64 address — corrupted pointer",
            }));
            return annotations;
        }
    }

    // ── 5. Kernel-space pointer (64-bit only) ──────────────────────────────
    // User-mode code should never hold a kernel-space address in a local
    // variable. On AMD64 the split is 0xFFFF_8000… and on ARM64 it is
    // 0x0000_8000… (48-bit VA).
    if ptr_size == 8 {
        let is_kernel = match arch {
            Arch::Amd64 => value >= 0xFFFF_8000_0000_0000,
            Arch::Arm64 => value >= 0x0000_8000_0000_0000,
            _ => false,
        };
        if is_kernel {
            annotations.push(json!({
                "type": "kernel_address",
                "description": "Kernel-space address in user-mode variable — corrupted pointer",
            }));
            return annotations;
        }
    }

    // ── 6. Partially overwritten pointer (64-bit) ──────────────────────────
    // If only the upper 32 bits contain a sentinel but the lower 32 don't,
    // a 32-bit write likely clobbered half the pointer (buffer overflow,
    // type confusion, etc.).
    if ptr_size == 8 {
        let high32 = (value >> 32) as u32;
        let low32 = value as u32;
        for sentinel in SENTINELS {
            if high32 == sentinel.pattern32 && low32 != sentinel.pattern32 {
                annotations.push(json!({
                    "type": "partial_overwrite",
                    "pattern": sentinel.kind,
                    "description": format!(
                        "Partially overwritten pointer — high bytes contain {} sentinel",
                        sentinel.kind,
                    ),
                }));
                return annotations;
            }
        }
    }

    // ── 7. Sentinel-filled dereferenced memory ─────────────────────────────
    // If the pointed-to memory is readable and filled with a known sentinel
    // pattern, the allocation was likely freed / uninitialised.
    // We intentionally do NOT flag unmapped addresses: MiniDumpNormal and
    // sentry-native SMART mode only capture stack + crash-adjacent heap, so
    // most valid heap pointers would be false-positived as "unmapped".
    if let Some(bytes) = memory.read_bytes(value, 16) {
        if bytes.len() >= 8 {
            let first4 = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            let second4 = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
            // Two consecutive identical 32-bit words → likely filled.
            if first4 == second4 {
                for sentinel in SENTINELS {
                    if first4 == sentinel.pattern32 {
                        annotations.push(json!({
                            "type": "sentinel_value",
                            "pattern": format!("{}_deref", sentinel.kind),
                            "description": format!(
                                "Points to memory filled with {} pattern — {}",
                                sentinel.kind, sentinel.description,
                            ),
                        }));
                        return annotations;
                    }
                }
            }
        }
    }

    annotations
}

// ── DWARF register number → minidump register name ─────────────────────────

fn dwarf_register_name(reg: u16, arch: Arch) -> Option<&'static str> {
    match arch {
        Arch::Amd64 => match reg {
            0 => Some("rax"), 1 => Some("rdx"), 2 => Some("rcx"), 3 => Some("rbx"),
            4 => Some("rsi"), 5 => Some("rdi"), 6 => Some("rbp"), 7 => Some("rsp"),
            8 => Some("r8"),  9 => Some("r9"),  10 => Some("r10"), 11 => Some("r11"),
            12 => Some("r12"), 13 => Some("r13"), 14 => Some("r14"), 15 => Some("r15"),
            16 => Some("rip"),
            _ => None,
        },
        Arch::X86 => match reg {
            0 => Some("eax"), 1 => Some("ecx"), 2 => Some("edx"), 3 => Some("ebx"),
            4 => Some("esp"), 5 => Some("ebp"), 6 => Some("esi"), 7 => Some("edi"),
            8 => Some("eip"),
            _ => None,
        },
        Arch::Arm64 => match reg {
            0..=28 => {
                const NAMES: [&str; 29] = [
                    "x0","x1","x2","x3","x4","x5","x6","x7","x8","x9",
                    "x10","x11","x12","x13","x14","x15","x16","x17","x18","x19",
                    "x20","x21","x22","x23","x24","x25","x26","x27","x28",
                ];
                Some(NAMES[reg as usize])
            }
            29 => Some("fp"),
            30 => Some("lr"),
            31 => Some("sp"),
            _ => None,
        },
        Arch::Arm => match reg {
            0..=15 => {
                const NAMES: [&str; 16] = [
                    "r0","r1","r2","r3","r4","r5","r6","r7","r8","r9",
                    "r10","r11","r12","sp","lr","pc",
                ];
                Some(NAMES[reg as usize])
            }
            _ => None,
        },
        _ => None,
    }
}

/// The default frame base register for `FrameOffset` (DW_OP_fbreg).
/// In debug builds (-O0), the frame base is typically the frame pointer register.
fn frame_base_register(arch: Arch) -> Option<&'static str> {
    match arch {
        Arch::Amd64 => Some("rbp"),
        Arch::X86 => Some("ebp"),
        Arch::Arm64 => Some("fp"),
        Arch::Arm => Some("r11"),
        _ => None,
    }
}

fn pointer_size(arch: Arch) -> usize {
    match arch {
        Arch::X86 | Arch::Arm | Arch::Ppc | Arch::Mips => 4,
        _ => 8,
    }
}

/// Encode bytes as a hex string (without allocating the `hex` crate).
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── Value evaluation ────────────────────────────────────────────────────────

/// Resolve the memory address where a variable is stored, given its location
/// and the register state for the frame.
fn resolve_variable_address(
    location: &VariableLocation,
    registers: &HashMap<String, u64>,
    arch: Arch,
    instruction_addr: u64,
) -> Option<VariableSource> {
    match location {
        VariableLocation::Register(reg) => {
            let name = dwarf_register_name(*reg, arch)?;
            let value = registers.get(name)?;
            Some(VariableSource::Register(*value))
        }
        VariableLocation::FrameOffset(offset) => {
            let fb_name = frame_base_register(arch)?;
            let fb_value = registers.get(fb_name)?;
            let addr = (*fb_value as i64 + offset) as u64;
            Some(VariableSource::Memory(addr))
        }
        VariableLocation::RegisterRelative { register, offset } => {
            let name = dwarf_register_name(*register, arch)?;
            let reg_value = match registers.get(name) {
                Some(v) => v,
                None => {
                    tracing::debug!(
                        "    register {:?} (dwarf #{}) not found in frame registers. Available: {:?}",
                        name,
                        register,
                        registers.keys().collect::<Vec<_>>(),
                    );
                    return None;
                }
            };
            let addr = (*reg_value as i64 + offset) as u64;
            tracing::debug!(
                "    resolved RegisterRelative(reg={}, off={}) → {}+{} = 0x{:x}",
                name, offset, reg_value, offset, addr,
            );
            Some(VariableSource::Memory(addr))
        }
        VariableLocation::LocationList(entries) => {
            // Find the entry whose PC range contains the current instruction address.
            for (start, end, inner_loc) in entries {
                if instruction_addr >= *start && instruction_addr < *end {
                    return resolve_variable_address(inner_loc, registers, arch, instruction_addr);
                }
            }
            None
        }
        VariableLocation::OptimizedOut => None,
        VariableLocation::Expression(_) => {
            // Full DWARF expression evaluation would require a stack machine.
            // Skipping for now — complex expressions are rare in -O0 builds.
            None
        }
    }
}

enum VariableSource {
    /// Value lives entirely in a register (the u64 IS the value).
    Register(u64),
    /// Value lives in memory at this address.
    Memory(u64),
}

/// Read raw bytes for a variable, either from a register or from memory.
fn read_variable_bytes(
    source: &VariableSource,
    byte_size: usize,
    memory: &MinidumpMemorySnapshot,
) -> Option<Vec<u8>> {
    match source {
        VariableSource::Register(value) => {
            // Register value: take the low `byte_size` bytes.
            let full = value.to_le_bytes();
            if byte_size <= 8 {
                Some(full[..byte_size].to_vec())
            } else {
                None
            }
        }
        VariableSource::Memory(addr) => {
            let result = memory.read_bytes(*addr, byte_size);
            if result.is_none() {
                tracing::debug!(
                    "    memory read FAILED at 0x{:x} size={}, regions: {}",
                    addr,
                    byte_size,
                    memory.regions.iter()
                        .map(|r| format!("[0x{:x}..0x{:x}]", r.start_addr, r.start_addr + r.data.len() as u64))
                        .take(5)
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            result
        }
    }
}

/// Maximum recursion depth for struct field evaluation.
const MAX_EVAL_DEPTH: usize = 3;
/// Maximum number of array elements to show.
const MAX_ARRAY_ELEMENTS: usize = 8;
/// Maximum total size of the vars payload per event (bytes of JSON).
const MAX_VARS_PAYLOAD_SIZE: usize = 64 * 1024;

/// Interpret raw bytes as a typed value and return a JSON representation.
fn interpret_value(
    type_info: &VariableType,
    bytes: &[u8],
    memory: &MinidumpMemorySnapshot,
    depth: usize,
) -> JsonValue {
    match type_info {
        VariableType::Primitive { encoding, byte_size } => {
            interpret_primitive(*encoding, *byte_size, bytes)
        }
        VariableType::Pointer { pointee_type_name, byte_size } => {
            interpret_pointer(pointee_type_name, *byte_size, bytes, memory)
        }
        VariableType::Struct { fields, .. } if depth < MAX_EVAL_DEPTH => {
            interpret_struct(fields, bytes, memory, depth)
        }
        VariableType::Enum { variants, byte_size, .. } => {
            interpret_enum(variants, *byte_size, bytes)
        }
        VariableType::Array { element_type_name, element_type, count, byte_size } => {
            interpret_array(element_type_name, element_type, *count, *byte_size, bytes, memory, depth)
        }
        _ => {
            // Struct at max depth, Unknown, etc.
            JsonValue::String(format!("0x{}", bytes_to_hex(bytes)))
        }
    }
}

fn interpret_primitive(encoding: PrimitiveEncoding, byte_size: u16, bytes: &[u8]) -> JsonValue {
    if bytes.len() < byte_size as usize {
        return JsonValue::Null;
    }
    match (encoding, byte_size) {
        (PrimitiveEncoding::SignedInt, 1) => json!(i8::from_le_bytes(bytes[..1].try_into().unwrap())),
        (PrimitiveEncoding::SignedInt, 2) => json!(i16::from_le_bytes(bytes[..2].try_into().unwrap())),
        (PrimitiveEncoding::SignedInt, 4) => json!(i32::from_le_bytes(bytes[..4].try_into().unwrap())),
        (PrimitiveEncoding::SignedInt, 8) => json!(i64::from_le_bytes(bytes[..8].try_into().unwrap())),
        (PrimitiveEncoding::UnsignedInt | PrimitiveEncoding::UnsignedChar, 1) => json!(bytes[0]),
        (PrimitiveEncoding::UnsignedInt, 2) => json!(u16::from_le_bytes(bytes[..2].try_into().unwrap())),
        (PrimitiveEncoding::UnsignedInt, 4) => json!(u32::from_le_bytes(bytes[..4].try_into().unwrap())),
        (PrimitiveEncoding::UnsignedInt, 8) => json!(u64::from_le_bytes(bytes[..8].try_into().unwrap())),
        (PrimitiveEncoding::Float, 4) => {
            let f = f32::from_le_bytes(bytes[..4].try_into().unwrap());
            json!(f)
        }
        (PrimitiveEncoding::Float, 8) => {
            let f = f64::from_le_bytes(bytes[..8].try_into().unwrap());
            json!(f)
        }
        (PrimitiveEncoding::Boolean, _) => json!(bytes[0] != 0),
        (PrimitiveEncoding::Char, 1) => {
            let ch = bytes[0];
            if ch.is_ascii_graphic() || ch == b' ' {
                json!(format!("'{}'", ch as char))
            } else {
                json!(format!("'\\x{:02x}'", ch))
            }
        }
        _ => json!(format!("0x{}", bytes_to_hex(&bytes[..byte_size as usize]))),
    }
}

fn interpret_pointer(
    _pointee_type_name: &str,
    byte_size: u16,
    bytes: &[u8],
    memory: &MinidumpMemorySnapshot,
) -> JsonValue {
    let ptr_val = if byte_size == 4 && bytes.len() >= 4 {
        u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64
    } else if bytes.len() >= 8 {
        u64::from_le_bytes(bytes[..8].try_into().unwrap())
    } else {
        return JsonValue::Null;
    };

    let hex_val = format!("0x{:x}", ptr_val);
    let annotations = check_pointer_annotations(ptr_val, memory);

    // Try to read a C string if it's a char* and not corrupt.
    let mut result = json!({ "__value": hex_val });
    if annotations.is_empty() && ptr_val != 0 {
        if let Some(s) = try_read_c_string(memory, ptr_val, 128) {
            result["__string_value"] = json!(s);
        }
    }
    if !annotations.is_empty() {
        result["__annotations"] = json!(annotations);
    }
    result
}

fn interpret_struct(
    fields: &[symbolic::debuginfo::StructField],
    bytes: &[u8],
    memory: &MinidumpMemorySnapshot,
    depth: usize,
) -> JsonValue {
    let mut obj = serde_json::Map::new();
    for field in fields.iter().take(16) {
        let start = field.offset as usize;
        let end = start + field.byte_size as usize;
        if end <= bytes.len() {
            let field_value = interpret_value(&field.type_info, &bytes[start..end], memory, depth + 1);
            obj.insert(field.name.clone(), flatten_to_display(field_value));
        }
    }
    JsonValue::Object(obj)
}

/// Collapse a rich metadata object into a simple display value.
///
/// Relay's event normalization stringifies nested objects when they exceed
/// the depth limit, turning `{"__string_value": "Alice"}` into
/// `"{\"__string_value\":\"Alice\"}"`. To avoid this, struct field values
/// are flattened to simple JSON scalars before being returned.
fn flatten_to_display(value: JsonValue) -> JsonValue {
    match &value {
        JsonValue::Object(map) => {
            if let Some(JsonValue::String(s)) = map.get("__string_value") {
                return JsonValue::String(format!("\"{}\"", s));
            }
            if let Some(v) = map.get("__value") {
                return v.clone();
            }
            // Nested struct without metadata keys: format as {.field1 = val, ...}
            let parts: Vec<String> = map
                .iter()
                .filter(|(k, _)| !k.starts_with("__"))
                .map(|(k, v)| format!(".{} = {}", k, display_json_value(v)))
                .collect();
            JsonValue::String(format!("{{{}}}", parts.join(", ")))
        }
        _ => value,
    }
}

fn display_json_value(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => s.clone(),
        JsonValue::Number(n) => n.to_string(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn interpret_enum(variants: &[(String, i64)], byte_size: u16, bytes: &[u8]) -> JsonValue {
    let raw_val = match byte_size {
        1 if bytes.len() >= 1 => bytes[0] as i64,
        2 if bytes.len() >= 2 => i16::from_le_bytes(bytes[..2].try_into().unwrap()) as i64,
        4 if bytes.len() >= 4 => i32::from_le_bytes(bytes[..4].try_into().unwrap()) as i64,
        8 if bytes.len() >= 8 => i64::from_le_bytes(bytes[..8].try_into().unwrap()),
        _ => return JsonValue::Null,
    };
    for (name, val) in variants {
        if *val == raw_val {
            return json!(name);
        }
    }
    json!(raw_val)
}

fn interpret_array(
    element_type_name: &str,
    element_type: &VariableType,
    count: u64,
    byte_size: u32,
    bytes: &[u8],
    memory: &MinidumpMemorySnapshot,
    depth: usize,
) -> JsonValue {
    if count == 0 || byte_size == 0 {
        return json!([]);
    }
    let elem_size = byte_size as u64 / count;

    // char arrays: try to interpret as a NUL-terminated C string.
    if elem_size == 1
        && matches!(element_type_name, "char" | "signed char" | "unsigned char")
    {
        let len = bytes.len().min(count as usize);
        let str_bytes = &bytes[..len];
        let end = str_bytes.iter().position(|&b| b == 0).unwrap_or(len);
        if let Ok(s) = std::str::from_utf8(&str_bytes[..end]) {
            if !s.is_empty() {
                return json!({ "__string_value": s });
            }
        }
    }

    let show = (count as usize).min(MAX_ARRAY_ELEMENTS);
    let mut arr = Vec::with_capacity(show);
    for i in 0..show {
        let start = i * elem_size as usize;
        let end = start + elem_size as usize;
        if end <= bytes.len() {
            arr.push(interpret_value(element_type, &bytes[start..end], memory, depth + 1));
        }
    }
    if count as usize > MAX_ARRAY_ELEMENTS {
        arr.push(json!(format!("... ({} more)", count as usize - MAX_ARRAY_ELEMENTS)));
    }
    json!(arr)
}

/// Try to read a NUL-terminated C string from memory.
fn try_read_c_string(memory: &MinidumpMemorySnapshot, addr: u64, max_len: usize) -> Option<String> {
    let bytes = memory.read_bytes(addr, max_len)?;
    let nul_pos = bytes.iter().position(|&b| b == 0)?;
    let s = std::str::from_utf8(&bytes[..nul_pos]).ok()?;
    if s.is_empty() {
        return None;
    }
    Some(s.to_string())
}

// ── Main extraction entry point ─────────────────────────────────────────────

/// Populates `frame.raw.vars` with variable names, types, and **runtime values**.
///
/// For each frame (up to `max_frames` per thread), looks up the function in the
/// debug session, resolves each variable's location against the frame's register
/// state, reads the value from minidump memory, and interprets it according to
/// the variable's type information.
pub fn extract_variables(
    module_lookup: &ModuleLookup,
    debug_sessions: &DebugSessions<'_>,
    stacktraces: &mut [CompleteStacktrace],
    memory: &MinidumpMemorySnapshot,
    max_frames: usize,
) {
    let mut total_payload_size: usize = 0;
    let mut total_vars_found = 0usize;

    tracing::debug!(
        "Starting variable extraction: {} stacktraces, max_frames={}",
        stacktraces.len(),
        max_frames,
    );

    for (thread_idx, trace) in stacktraces.iter_mut().enumerate() {
        let mut frames_processed = 0;
        tracing::debug!(
            "Thread {}: {} frames to process",
            thread_idx,
            trace.frames.len(),
        );
        for frame in &mut trace.frames {
            if frames_processed >= max_frames || total_payload_size >= MAX_VARS_PAYLOAD_SIZE {
                break;
            }

            // Look up per-frame registers from the memory snapshot.
            let frame_idx = frame.original_index.unwrap_or(frames_processed);
            let registers = memory
                .thread_frame_registers
                .get(thread_idx)
                .and_then(|frames| frames.get(frame_idx));

            if let Some(regs) = registers {
                tracing::debug!(
                    "Frame {} (func={:?}, addr=0x{:x}): {} registers available",
                    frames_processed,
                    frame.raw.function.as_deref().unwrap_or("<unknown>"),
                    frame.raw.instruction_addr.0,
                    regs.len(),
                );
                if let Some(vars) = extract_frame_variables(
                    module_lookup,
                    debug_sessions,
                    frame,
                    memory,
                    regs,
                ) {
                    let size = vars.to_string().len();
                    let var_count = vars.as_object().map_or(0, |o| o.len());
                    total_vars_found += var_count;
                    tracing::info!(
                        "Frame {} (func={:?}): extracted {} variables ({} bytes)",
                        frames_processed,
                        frame.raw.function.as_deref().unwrap_or("<unknown>"),
                        var_count,
                        size,
                    );
                    if total_payload_size + size <= MAX_VARS_PAYLOAD_SIZE {
                        total_payload_size += size;
                        frame.raw.vars = Some(vars);
                    } else {
                        tracing::warn!("Skipping vars for frame {}: would exceed payload limit", frames_processed);
                    }
                }
            } else {
                tracing::debug!(
                    "Frame {} (func={:?}): no registers available (thread_idx={}, frame_idx={})",
                    frames_processed,
                    frame.raw.function.as_deref().unwrap_or("<unknown>"),
                    thread_idx,
                    frame_idx,
                );
            }

            frames_processed += 1;
        }
    }

    tracing::info!(
        "Variable extraction complete: {} total variables found, {} bytes payload",
        total_vars_found,
        total_payload_size,
    );
}

/// Extracts variable values for a single symbolicated frame.
fn extract_frame_variables(
    module_lookup: &ModuleLookup,
    debug_sessions: &DebugSessions<'_>,
    frame: &SymbolicatedFrame,
    memory: &MinidumpMemorySnapshot,
    registers: &HashMap<String, u64>,
) -> Option<JsonValue> {
    let addr = frame.raw.instruction_addr.0;
    let addr_mode = frame.raw.addr_mode;

    let (module_index, relative_addr) = match module_lookup.get_module_addr(addr, addr_mode) {
        Some(v) => v,
        None => {
            tracing::debug!("  No module found for addr 0x{:x}", addr);
            return None;
        }
    };

    let session = match debug_sessions.get(&module_index) {
        Some(Some((_scope, session))) => session,
        _ => {
            tracing::debug!("  No debug session for module_index={}", module_index);
            return None;
        }
    };

    tracing::debug!(
        "  Looking up variables at relative_addr=0x{:x} in module_index={}",
        relative_addr,
        module_index,
    );

    let variables = match find_variables_at_addr(session, relative_addr) {
        Some(v) => v,
        None => {
            tracing::debug!("  No variables found at addr 0x{:x}", relative_addr);
            return None;
        }
    };

    if variables.is_empty() {
        tracing::debug!("  Variables list empty at addr 0x{:x}", relative_addr);
        return None;
    }

    tracing::debug!("  Found {} variables at addr 0x{:x}", variables.len(), relative_addr);
    for v in &variables {
        tracing::debug!(
            "    var: name={:?}, type={:?}, param={}, location={:?}",
            v.name, v.type_name, v.is_parameter, v.location,
        );
    }

    let mut vars = serde_json::Map::new();
    for (order, var_info) in variables.iter().enumerate() {
        let mut value = evaluate_variable(var_info, registers, memory, relative_addr);
        if let JsonValue::Object(ref mut map) = value {
            map.insert("__order".to_string(), JsonValue::Number(order.into()));
        }
        tracing::debug!("    evaluated {:?} = {}", var_info.name, value);
        vars.insert(var_info.name.clone(), value);
    }

    if vars.is_empty() {
        None
    } else {
        Some(JsonValue::Object(vars))
    }
}

/// All the info we need about a variable for evaluation.
struct ExtractedVariable {
    name: String,
    type_name: String,
    type_info: VariableType,
    is_parameter: bool,
    location: VariableLocation,
}

/// Evaluate a single variable: resolve location → read bytes → interpret value.
fn evaluate_variable(
    var: &ExtractedVariable,
    registers: &HashMap<String, u64>,
    memory: &MinidumpMemorySnapshot,
    instruction_addr: u64,
) -> JsonValue {
    if matches!(var.location, VariableLocation::OptimizedOut) {
        return json!({
            "__type": &var.type_name,
            "__is_parameter": var.is_parameter,
            "__status": "optimized_out",
        });
    }

    let byte_size = var.type_info.byte_size().unwrap_or(0) as usize;
    if byte_size == 0 {
        return json!({
            "__type": &var.type_name,
            "__is_parameter": var.is_parameter,
            "__status": "unknown_size",
        });
    }

    let source = resolve_variable_address(
        &var.location,
        registers,
        memory.arch,
        instruction_addr,
    );

    let source = match source {
        Some(s) => s,
        None => {
            return json!({
                "__type": &var.type_name,
                "__is_parameter": var.is_parameter,
                "__status": "unresolvable_location",
            });
        }
    };

    let bytes = match read_variable_bytes(&source, byte_size, memory) {
        Some(b) => b,
        None => {
            return json!({
                "__type": &var.type_name,
                "__is_parameter": var.is_parameter,
                "__status": "memory_unavailable",
            });
        }
    };

    let value = interpret_value(&var.type_info, &bytes, memory, 0);

    // Wrap with metadata.
    let mut result = json!({
        "__type": &var.type_name,
        "__is_parameter": var.is_parameter,
    });

    // If the interpreted value is an object (struct/pointer with metadata),
    // merge its fields into our result.
    if let JsonValue::Object(map) = &value {
        if let JsonValue::Object(ref mut res) = result {
            for (k, v) in map {
                res.insert(k.clone(), v.clone());
            }
        }
    } else {
        result["__value"] = value;
    }

    result
}

// ── Debug session variable lookup ───────────────────────────────────────────

/// Searches for variables at a given address within a debug session.
fn find_variables_at_addr(
    session: &ObjectDebugSession<'_>,
    addr: u64,
) -> Option<Vec<ExtractedVariable>> {
    let mut result = Vec::new();
    let functions: Vec<_> = session.functions().filter_map(|f| f.ok()).collect();

    tracing::debug!(
        "  find_variables_at_addr: iterating {} functions for addr 0x{:x}",
        functions.len(),
        addr,
    );

    for func in &functions {
        if !contains_addr(func, addr) {
            continue;
        }

        tracing::debug!(
            "  Found matching function {:?} (addr=0x{:x}, size=0x{:x}, {} variables, {} inlinees)",
            func.name.as_str(),
            func.address,
            func.size,
            func.variables.len(),
            func.inlinees.len(),
        );

        // Collect variables from the function itself.
        collect_variables(&func.variables, &mut result);

        // Also check inlined functions.
        for inlinee in &func.inlinees {
            if contains_addr(inlinee, addr) {
                tracing::debug!(
                    "  Matched inlinee {:?} ({} variables)",
                    inlinee.name.as_str(),
                    inlinee.variables.len(),
                );
                collect_variables(&inlinee.variables, &mut result);
            }
        }

        if !result.is_empty() {
            return Some(result);
        }
    }

    tracing::debug!("  No variables found for addr 0x{:x}", addr);
    if result.is_empty() { None } else { Some(result) }
}

fn collect_variables(
    vars: &[symbolic::debuginfo::Variable<'_>],
    out: &mut Vec<ExtractedVariable>,
) {
    for var in vars {
        out.push(ExtractedVariable {
            name: var.name.to_string(),
            type_name: var.type_name.to_string(),
            type_info: var.type_info.clone(),
            is_parameter: var.is_parameter,
            location: var.location.clone(),
        });
    }
}

/// Checks whether a function's address range contains the given address.
fn contains_addr(func: &Function<'_>, addr: u64) -> bool {
    addr >= func.address && addr < func.address.saturating_add(func.size)
}
