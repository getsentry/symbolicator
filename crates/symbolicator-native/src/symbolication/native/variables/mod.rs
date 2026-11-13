//! Module containing logic for extracting variable values from native crashes.

use std::collections::BTreeMap;

use symbolic::common::register::Register;
use symbolic::symcache::{SourceLocation, SymCache, Type, VariableLocation, VariableLocationInfo};
use symbolicator_service::utils::hex::HexValue;

use crate::interface::{VariableValue, Variables};
use crate::memory::{ArrayMemoryAccess, Endian, MemoryAccess, MemoryAccessExt};

mod formatting;

/// Perform variable extraction.
///
/// This function extracts all the variables we can based on the information provided.
/// Variables whose value cannot be determined are still included, without a value.
///
/// `cfa` is the Canonical Frame Address of the frame. Variables located relative to the CFA have
/// no value when it is `None`.
pub fn do_extract<'data, 'cache>(
    source_location: &SourceLocation<'data, 'cache>,
    cache: &SymCache<'cache>,
    registers: &BTreeMap<Register, HexValue>,
    cfa: Option<u64>,
    memory: &dyn MemoryAccess,
) -> Variables {
    source_location
        .variables()
        .flat_map(|variable| Some((variable.name()?, variable)))
        .map(|(name, variable)| {
            let value = variable
                .locations()
                .find_map(|loc| {
                    resolve_variable_value(cache, registers, cfa, memory, loc, variable.ty())
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
    cfa: Option<u64>,
    memory: &dyn MemoryAccess,
    location: VariableLocationInfo,
    ty: Option<Type<'_>>,
) -> Option<VariableValue> {
    let ty = ty?;

    let aov = match location.location {
        VariableLocation::Register { register } => registers
            .get(&register)
            .copied()
            .map(AddrOrValue::from_register)?,
        VariableLocation::RegisterOffset { register, offset } => {
            let &HexValue(base) = registers.get(&register)?;
            AddrOrValue::Addr(base.checked_add_signed(offset)?)
        }
        VariableLocation::CfaOffset { offset } => {
            AddrOrValue::Addr(cfa?.checked_add_signed(offset)?)
        }
    };

    formatting::format(cache, memory, aov, ty)
}

/// An address into the crash's memory or a static value, in for example a register.
#[derive(Debug, Clone, Copy)]
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
