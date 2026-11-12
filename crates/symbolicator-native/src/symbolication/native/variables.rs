//! Module containing logic for extracting variable values from native crashes.

use std::collections::BTreeMap;

use symbolic::common::CpuFamily;
use symbolic::common::register::{Amd64Register, Arm64Register, Register};
use symbolic::symcache::{SourceLocation, SymCache, Type, VariableLocation, VariableLocationInfo};
use symbolicator_service::utils::hex::HexValue;

use crate::interface::{VariableValue, Variables};
use crate::memory::{ArrayMemoryAccess, Endian, MemoryAccess, MemoryAccessExt};

mod formatting;

/// Perform variable extraction.
///
/// This function extracts all the variables we can based on the information provided.
/// Variables whose value cannot be determined are still included, without a value.
pub fn do_extract<'data, 'cache>(
    source_location: &SourceLocation<'data, 'cache>,
    cache: &SymCache<'cache>,
    registers: &BTreeMap<Register, HexValue>,
    memory: &dyn MemoryAccess,
) -> Variables {
    source_location
        .variables()
        .flat_map(|variable| Some((variable.name()?, variable)))
        .map(|(name, variable)| {
            let mut ty = String::new();
            resolve_type_name(&mut ty, cache, variable.ty(), 0);

            let value = variable
                .locations()
                .find_map(|loc| {
                    resolve_variable_value(cache, registers, memory, loc, variable.ty())
                })
                .unwrap_or_default();

            let interface_variable = value.into_variable(ty, variable.kind());

            (name, interface_variable)
        })
        .collect()
}

fn resolve_variable_value(
    cache: &SymCache<'_>,
    registers: &BTreeMap<Register, HexValue>,
    memory: &dyn MemoryAccess,
    location: VariableLocationInfo,
    ty: Option<Type<'_>>,
) -> Option<VariableValue> {
    let ty = ty?;

    match location.location {
        VariableLocation::Register { register } => registers
            .get(&register)
            .copied()
            .map(AddrOrValue::from_register)
            .and_then(|value| formatting::format(memory, value, ty)),
        VariableLocation::RegisterOffset { register, offset } => {
            let &HexValue(base) = registers.get(&register)?;
            let addr = base.checked_add_signed(offset)?;

            formatting::format(memory, AddrOrValue::Addr(addr), ty)
        }
        VariableLocation::FrameOffset { offset } => {
            let register = match cache.arch().cpu_family() {
                CpuFamily::Amd64 => Some(Amd64Register::Rbp.into()),
                CpuFamily::Arm64 => Some(Arm64Register::X29.into()),
                _ => None,
            }?;

            let &HexValue(base) = registers.get(&register)?;
            let addr = base.checked_add_signed(offset)?;

            formatting::format(memory, AddrOrValue::Addr(addr), ty)
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

    // This really is just temporary but necessary as a Symcache can serialize cyclic/recursive types.
    // With more changes we'll have to solve this properly.
    if depth > 10 {
        return;
    }

    match ty {
        Type::Primitive(primitive) => result.push_str(primitive.name().unwrap_or("")),
        Type::Pointer(pointer) if pointer.pointee().is_none() => result.push_str("void*"),
        Type::Pointer(pointer) => {
            let ty = pointer.pointee().and_then(|p| cache.lookup_type(p));
            resolve_type_name(result, cache, ty, depth + 1);
            result.push('*');
        }
        Type::TypeDef(typedef) => result.push_str(typedef.name()),
        Type::StructureType(st) => result.push_str(st.name().unwrap_or("<anonymous>")),
        _ => result.push_str("<not implemented>"),
    }
}

/// An address into the crash's memory or a static value, in for example a register.
#[derive(Debug)]
enum AddrOrValue {
    /// The value is stored at a specific address.
    Addr(u64),
    /// A little endian value.
    Value([u8; 8]),
}

impl AddrOrValue {
    /// Creates a [`AddrOrValue`] from a register value.
    fn from_register(HexValue(reg): HexValue) -> Self {
        Self::Value(reg.to_le_bytes())
    }

    /// Returns `size` bytes from the underlying memory.
    ///
    /// See also: [`MemoryAccess::get_memory_at_address`].
    fn get_memory<'a>(&'a self, memory: &'a dyn MemoryAccess, size: usize) -> Option<&'a [u8]> {
        match self {
            Self::Addr(addr) => memory.get_memory_at_address(*addr, size),
            Self::Value(array) => array.get(..size),
        }
    }

    /// Attempts to parse `T` from the underlying memory.
    ///
    /// See also: [`MemoryAccessExt::get_value_at_address`].
    fn get_value<T>(&self, memory: &dyn MemoryAccess) -> Option<T>
    where
        T: scroll::ctx::SizeWith<Endian>,
        for<'a> T: scroll::ctx::TryFromCtx<'a, Endian>,
    {
        match self {
            Self::Addr(addr) => memory.get_value_at_address(*addr),
            Self::Value(array) => {
                ArrayMemoryAccess::new(*array, Endian::Little).get_value_at_address(0)
            }
        }
    }
}
