//! Module containing logic for extracting variable values from native crashes.

use minidump::CpuContext;
use symbolic::common::CpuFamily;
use symbolic::symcache::{
    SourceLocation, SymCache, Type, TypeSize, VariableLocation, VariableLocationInfo,
};
use symbolicator_service::utils::hex::HexValue;

use crate::interface::{Registers, VariableValue, Variables};
use crate::memory::{MemoryAccess, MemoryAccessExt as _};

/// Perform variable extraction.
///
/// This function extracts all the variables we can based on the information provided.
pub(super) fn do_extract<'data, 'cache>(
    source_location: &SourceLocation<'data, 'cache>,
    cache: &SymCache<'cache>,
    registers: &Registers,
    memory: &dyn MemoryAccess,
) -> Variables {
    source_location
        .variables()
        .flat_map(|variable| {
            let name = variable.name()?;

            let mut ty = String::new();
            resolve_type_name(&mut ty, cache, variable.ty(), 0);

            let value = variable
                .locations()
                .find_map(|loc| resolve_value(cache, registers, memory, loc, variable.ty()));

            let interface_variable = VariableValue::new()
                .value(value)
                .ty(ty)
                .kind(variable.kind());

            Some((name, interface_variable))
        })
        .collect()
}

fn resolve_value(
    cache: &SymCache<'_>,
    registers: &Registers,
    memory: &dyn MemoryAccess,
    location: VariableLocationInfo,
    ty: Option<Type<'_>>,
) -> Option<String> {
    let TypeSize::Bytes(size) = match ty? {
        Type::Primitive(ty) => ty.size(),
        Type::Pointer(ty) => ty.size(),
        _ => return None,
    };

    match location.location {
        VariableLocation::Register { id } => {
            // Temporary hack, `symbolic` will need an abstraction over registers, which allows
            // mapping register names to the gimli register ids.
            match cache.arch().cpu_family() {
                CpuFamily::Amd64 => minidump::format::CONTEXT_AMD64::REGISTERS,
                CpuFamily::Arm64 => minidump::format::CONTEXT_ARM64::REGISTERS,
                _ => &[],
            }
            .get(id as usize)
            .and_then(|&reg| registers.get(reg))
            .map(|v| v.to_string())
        }
        VariableLocation::FrameOffset { offset } => {
            let &HexValue(frame_base) = match cache.arch().cpu_family() {
                CpuFamily::Amd64 => Some("rbp"),
                CpuFamily::Arm64 => Some("fp"),
                _ => None,
            }
            .and_then(|reg| registers.get(reg))?;

            let addr = u64::try_from(i64::try_from(frame_base).ok()? + offset).ok()?;

            // This obviously will need to be changed to consider the variable type.
            match size {
                1 => memory
                    .get_value_at_address::<u8>(addr)
                    .map(|v| HexValue(v.into()).to_string()),
                2 => memory
                    .get_value_at_address::<u16>(addr)
                    .map(|v| HexValue(v.into()).to_string()),
                4 => memory
                    .get_value_at_address::<u32>(addr)
                    .map(|v| HexValue(v.into()).to_string()),
                8 => memory
                    .get_value_at_address::<u64>(addr)
                    .map(|v| HexValue(v).to_string()),
                s => memory
                    .get_memory_at_address(addr, s as usize)
                    .map(|s| format!("{s:?}")),
            }
        }
    }
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
