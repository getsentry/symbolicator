//! Module containing logic for extracting variable values from native crashes.

use std::collections::BTreeMap;

use symbolic::common::CpuFamily;
use symbolic::common::register::{Amd64Register, Arm64Register, Register};
use symbolic::symcache::{SourceLocation, SymCache, Type, VariableLocation, VariableLocationInfo};
use symbolicator_service::utils::hex::HexValue;

use crate::interface::{VariableValue, Variables};
use crate::memory::{Endian, MemoryAccess, MemoryAccessExt};

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
            let value = variable
                .locations()
                .find_map(|loc| {
                    resolve_variable_value(cache, registers, memory, loc, variable.ty())
                })
                .unwrap_or_default();

            let interface_variable = value.into_variable(variable.kind());

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

    let register_bytes;
    let aov = match location.location {
        VariableLocation::Register { register } => {
            let &HexValue(value) = registers.get(&register)?;
            register_bytes = value.to_le_bytes();
            AddrOrValue::Value(&register_bytes)
        }
        VariableLocation::RegisterOffset { register, offset } => {
            let &HexValue(base) = registers.get(&register)?;
            AddrOrValue::Addr(base.checked_add_signed(offset)?)
        }
        VariableLocation::FrameOffset { offset } => {
            let register = match cache.arch().cpu_family() {
                CpuFamily::Amd64 => Some(Amd64Register::Rbp.into()),
                CpuFamily::Arm64 => Some(Arm64Register::X29.into()),
                _ => None,
            }?;

            let &HexValue(base) = registers.get(&register)?;
            AddrOrValue::Addr(base.checked_add_signed(offset)?)
        }
    };

    formatting::format(cache, memory, aov, ty)
}

/// An address into the crash's memory or a static value, in for example a register.
#[derive(Debug, Clone, Copy)]
enum AddrOrValue<'a> {
    /// The value is stored at a specific address.
    Addr(u64),
    /// A little endian value.
    Value(&'a [u8]),
}

impl<'a> AddrOrValue<'a> {
    /// Offsets this [`AddrOrValue`] by `offset` bytes.
    fn offset(self, offset: u64) -> Self {
        match self {
            Self::Addr(addr) => Self::Addr(addr.saturating_add(offset)),
            Self::Value(bytes) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX);
                Self::Value(bytes.get(start..).unwrap_or(&[]))
            }
        }
    }

    /// Returns `size` bytes from the underlying memory.
    ///
    /// See also: [`MemoryAccess::get_memory_at_address`].
    fn get_memory<'m>(&self, memory: &'m dyn MemoryAccess, size: usize) -> Option<&'m [u8]>
    where
        'a: 'm,
    {
        match self {
            Self::Addr(addr) => memory.get_memory_at_address(*addr, size),
            Self::Value(bytes) => bytes.get(..size),
        }
    }

    /// Attempts to parse `T` from the underlying memory.
    ///
    /// See also: [`MemoryAccessExt::get_value_at_address`].
    fn get_value<T>(&self, memory: &dyn MemoryAccess) -> Option<T>
    where
        T: scroll::ctx::SizeWith<Endian>,
        for<'b> T: scroll::ctx::TryFromCtx<'b, Endian>,
    {
        match self {
            Self::Addr(addr) => memory.get_value_at_address(*addr),
            Self::Value(bytes) => T::try_from_ctx(bytes, Endian::Little)
                .ok()
                .map(|(value, _)| value),
        }
    }
}
