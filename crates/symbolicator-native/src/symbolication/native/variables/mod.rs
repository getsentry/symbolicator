//! Module containing logic for extracting variable values from native crashes.

use std::collections::BTreeMap;

use symbolic::common::register::Register;
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
        VariableLocation::CfaOffset { offset } => {
            AddrOrValue::Addr(cfa?.checked_add_signed(offset)?)
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
    /// See also: [`MemoryAccessExt::get_memory_at_address`].
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

    /// Reads up to 128 bits starting at `offset` in bits.
    fn get_bits(&self, memory: &dyn MemoryAccess, offset: u64, size: u64) -> Option<RawBits> {
        if size == 0 || size > 128 {
            return None;
        }

        let endian = match self {
            Self::Addr(_) => memory.endian(),
            Self::Value(_) => Endian::Little,
        };

        let byte_offset = offset / 8;
        let bit_offset = offset % 8;

        let bytes = self
            .offset(byte_offset)
            .get_memory(memory, (bit_offset + size).div_ceil(8) as usize)?;

        let mut data = 0;
        let mut remaining = size;
        let mut current_bit_offset = bit_offset;

        for &byte in bytes {
            let width = remaining.min(8 - current_bit_offset);
            let mask = (1u128 << width) - 1;
            data = match endian {
                Endian::Little => {
                    let bits = u128::from(byte >> current_bit_offset) & mask;
                    data | (bits << (size - remaining))
                }
                Endian::Big => {
                    let bits = u128::from(byte >> (8 - current_bit_offset - width)) & mask;
                    (data << width) | bits
                }
            };
            remaining -= width;
            current_bit_offset = 0;
        }

        Some(RawBits {
            data,
            size: size as u8,
        })
    }
}

/// Bits read from memory.
#[derive(Debug, Copy, Clone)]
pub struct RawBits {
    /// The raw bits read from memory.
    data: u128,
    /// Size of the bits, must be <= 128.
    size: u8,
}

impl RawBits {
    /// Returns the [`RawBits`] as a `bool`.
    pub fn as_bool(self) -> bool {
        self.as_u128() != 0
    }

    /// Returns the [`RawBits`] as a `u128`.
    pub fn as_u128(self) -> u128 {
        self.data
    }

    /// Returns the [`RawBits`] as a `i128`.
    pub fn as_i128(self) -> i128 {
        debug_assert!(self.size <= 128);
        let shift = 128 - self.size;
        // This performs a sign extension, for example:
        // The 5 bit signed integer `0b11101` is 29 unsigned, but -3 signed.
        ((self.data << shift) as i128) >> shift
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::SliceMemory;

    use super::*;

    #[test]
    fn test_get_bits_from_address_little_endian() {
        let value = AddrOrValue::Addr(1);
        let memory = SliceMemory::new(&[0xff, 0x96, 0x5b, 0xc2], Endian::Little);

        let bits = value.get_bits(&memory, 0, 16).unwrap();
        assert_eq!(bits.as_u128(), 0x5b96);
        assert_eq!(bits.size, 16);

        let bits = value.get_bits(&memory, 11, 9).unwrap();
        assert_eq!(bits.as_u128(), 0x04b);
        assert_eq!(bits.size, 9);
    }

    #[test]
    fn test_get_bits_from_address_big_endian() {
        let value = AddrOrValue::Addr(1);
        let memory = SliceMemory::new(&[0xff, 0x96, 0x5b, 0xc2], Endian::Big);

        let bits = value.get_bits(&memory, 0, 16).unwrap();
        assert_eq!(bits.as_u128(), 0x965b);
        assert_eq!(bits.size, 16);

        let bits = value.get_bits(&memory, 11, 9).unwrap();
        assert_eq!(bits.as_u128(), 0x1bc);
        assert_eq!(bits.size, 9);
    }

    #[test]
    fn test_get_bits_from_value_is_little_endian() {
        let memory = SliceMemory::new(&[], Endian::Big);
        let value = AddrOrValue::Value(&[0xff, 0x5b, 0xc2]);

        let bits = value.get_bits(&memory, 11, 9).unwrap();
        assert_eq!(bits.as_u128(), 0x04b);
        assert_eq!(bits.size, 9);
    }

    #[test]
    fn test_get_bits_bounds_from_address() {
        let memory = SliceMemory::new(&[0xff; 17], Endian::Big);
        let value = AddrOrValue::Addr(0);

        let bits = value.get_bits(&memory, 1, 128).unwrap();
        assert_eq!(bits.as_u128(), u128::MAX);

        assert!(value.get_bits(&memory, 0, 0).is_none());
        assert!(value.get_bits(&memory, 0, 129).is_none());
        assert!(value.get_bits(&memory, 135, 2).is_none());
    }

    #[test]
    fn test_get_bits_bounds_from_value() {
        let memory = SliceMemory::new(&[], Endian::Big);
        let value = AddrOrValue::Value(&[0xff; 17]);

        let bits = value.get_bits(&memory, 1, 128).unwrap();
        assert_eq!(bits.as_u128(), u128::MAX);

        assert!(value.get_bits(&memory, 0, 0).is_none());
        assert!(value.get_bits(&memory, 0, 129).is_none());
        assert!(value.get_bits(&memory, 135, 2).is_none());
    }

    #[test]
    fn test_raw_bits_signed_unsigned() {
        let bits = RawBits {
            data: 0b11101,
            size: 5,
        };

        assert!(bits.as_bool());
        assert_eq!(bits.as_u128(), 29);
        assert_eq!(bits.as_i128(), -3);
    }
}
