//! Module containing logic for extracting variable values from native crashes.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use serde_json::Value;
use symbolic::common::CpuFamily;
use symbolic::common::register::{Amd64Register, Arm64Register, Register};
use symbolic::symcache::{
    PrimitiveTypeEncoding, SourceLocation, SymCache, Type, TypeSize, VariableLocation,
    VariableLocationInfo,
};
use symbolicator_service::utils::hex::HexValue;

use crate::interface::{VariableValue, Variables};
use crate::memory::{ArrayMemoryAccess, Endian, MemoryAccess, MemoryAccessExt};

/// Perform variable extraction.
///
/// This function extracts all the variables we can based on the information provided.
/// Variables whose value cannot be determined are still included, without a value.
pub(super) fn do_extract<'data, 'cache>(
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
            .and_then(|v| format_value(memory, v, ty)),
        VariableLocation::RegisterOffset { register, offset } => {
            let &HexValue(base) = registers.get(&register)?;
            let addr = base.checked_add_signed(offset)?;

            format_value(memory, AddrOrValue::Addr(addr), ty)
        }
        VariableLocation::FrameOffset { offset } => {
            let register = match cache.arch().cpu_family() {
                CpuFamily::Amd64 => Some(Amd64Register::Rbp.into()),
                CpuFamily::Arm64 => Some(Arm64Register::X29.into()),
                _ => None,
            }?;

            let &HexValue(base) = registers.get(&register)?;
            let addr = base.checked_add_signed(offset)?;

            format_value(memory, AddrOrValue::Addr(addr), ty)
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

fn format_value(
    memory: &dyn MemoryAccess,
    value: AddrOrValue,
    ty: Type<'_>,
) -> Option<VariableValue> {
    use PrimitiveTypeEncoding::*;
    use TypeEncoding::*;

    /// Helper enum for the match statement.
    enum TypeEncoding {
        Primitive(PrimitiveTypeEncoding),
        /// A primitive with an unknown/unavailable encoding.
        UnknownPrimitive,
        Pointer,
    }

    /// Helper to convert the memory at addr to the provided type, and map this to a VariableValue.
    macro_rules! get_value {
        // If we just get a type, map this to a formatted variable value.
        ($ty:ty) => {
            get_value!($ty, |v| VariableValue::new().value(v))
        };

        // Also allow a custom mapping from value to VariableValue, as not everything will map
        // directly.
        ($ty:ty, $($mapping:tt)*) => {
            value.get_value::<$ty>(memory).map($($mapping)*)
        };
    }

    /// Helper macro to map a bool or unsigned int to a variable value.
    ///
    /// Not convenient to make this a function because the trait bounds are unclear.
    macro_rules! map_bool_unsigned {
        ($encoding:ident) => {
            |v| match $encoding {
                Boolean => VariableValue::new().value(v > 0),
                _ => VariableValue::new().value(v),
            }
        };
    }

    /// Create a formatted [`VariableValue`] according to the specified formatting.
    macro_rules! format_variable {
        ($($tt:tt)*) => {
            VariableValue::new().value(format!($($tt)*))
        };
    }

    let TypeSize::Bytes(size) = match &ty {
        Type::Primitive(ty) => ty.size(),
        Type::Pointer(ty) => ty.size(),
        _ => return None,
    };

    let encoding = match &ty {
        Type::Primitive(ty) => ty.encoding().map_or(UnknownPrimitive, Primitive),
        Type::Pointer(_) => Pointer,
        _ => return None,
    };

    match (encoding, size) {
        // Not sure this is realistic, but size 0 probably should map to an empty value.
        // Returning `None` could also make sense.
        (_, 0) => Some(VariableValue::new()),

        // Booleans and Unsigned ints.
        //
        // These need to be handled together because bool cannot be read directly from memory; we
        // instead read the Unsigned integer type and compare against 0 when this is a Boolean.
        (Primitive(encoding @ (Boolean | UnsignedInt)), 1) => {
            get_value!(u8, map_bool_unsigned!(encoding))
        }
        // Characters more than one byte are handled as integers.
        (Primitive(encoding @ (Boolean | UnsignedInt | UnsignedChar)), 2) => {
            get_value!(u16, map_bool_unsigned!(encoding))
        }
        (Primitive(encoding @ (Boolean | UnsignedInt | UnsignedChar)), 4) => {
            get_value!(u32, map_bool_unsigned!(encoding))
        }
        (Primitive(encoding @ (Boolean | UnsignedInt | UnsignedChar)), 8) => {
            get_value!(u64, map_bool_unsigned!(encoding))
        }
        (Primitive(encoding @ (Boolean | UnsignedInt | UnsignedChar)), 16) => {
            get_value!(u128, |v| match encoding {
                Boolean => VariableValue::new().value(v > 0),
                // Serializing u128 as a number is possible, but because other parts of Sentry may
                // not handle it gracefully due to the fact that the `arbitrary_precision` serde
                // feature is required, so we serialize u128's that cannot be represented as u64 as
                // a string instead.
                _ => match u64::try_from(v) {
                    Ok(v_u64) => VariableValue::new().value(v_u64),
                    Err(_) => format_variable!("{v}"),
                },
            })
        }

        // Signed integers. This is pretty simple, just get the value from memory.
        (Primitive(SignedInt), 1) => get_value!(i8),
        (Primitive(SignedInt | SignedChar), 2) => get_value!(i16),
        (Primitive(SignedInt | SignedChar), 4) => get_value!(i32),
        (Primitive(SignedInt | SignedChar), 8) => get_value!(i64),
        (Primitive(SignedInt | SignedChar), 16) => {
            // Serializing i128 as a number is possible, but because other parts of Sentry may
            // not handle it gracefully due to the fact that the `arbitrary_precision` serde
            // feature is required, so we serialize i128's that cannot be represented as i64 as
            // a string instead.
            get_value!(i128, |v| match i64::try_from(v) {
                Ok(v_i64) => VariableValue::new().value(v_i64),
                Err(_) => format_variable!("{v}"),
            })
        }

        // Floating-point types. Reading f128 from memory is currently not supported, so we don't
        // handle it for now.
        (Primitive(Float), 4) => get_value!(f32),
        (Primitive(Float), 8) => get_value!(f64),

        // 1-byte characters. Multibyte characters handled as integers.
        (Primitive(SignedChar | UnsignedChar), 1) => {
            get_value!(u8, |v| format_variable!("'{}'", v.escape_ascii()))
        }

        // Handle memory addresses by rending to a hexadecimal string.
        (Primitive(Address), 1) => get_value!(u8, |v| format_variable!("{v:#04x}")),
        (Primitive(Address), 2) => get_value!(u16, |v| format_variable!("{v:#06x}")),
        (Primitive(Address), 4) => get_value!(u32, |v| format_variable!("{v:#010x}")),
        (Primitive(Address), 8) => get_value!(u64, |v| format_variable!("{v:#018x}")),
        (Primitive(Address), 16) => get_value!(u128, |v| format_variable!("{v:#034x}")),

        // Pointers map to `pointer_address`. We assume only 32-bit and 64-bit pointer addresses
        // exist. We can of course support more if needed.
        (Pointer, 4) => get_value!(u32, |v| VariableValue::new().pointer_address(v)),
        (Pointer, 8) => get_value!(u64, |v| VariableValue::new().pointer_address(v)),

        // Unknown/unhandled data types.
        // We limit the size of memory slice we extract to 16 via UnknownBytes.
        (_, size) => size
            .try_into()
            .ok()
            .and_then(|size| value.get_memory(memory, size))
            .map(|v| VariableValue::new().value(UnknownBytes::from(v))),
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

/// An array of bytes without further meaning, used for variables when we cannot determine type.
///
/// This struct handles truncating the bytes to at most 16.
///
/// This provides a conversion into [`serde_json::Value`]. That conversion maps to an array, where
/// each element is a string containing the hexadecimal represenation of the byte. It also handles
/// including information about an amount of values truncated.
struct UnknownBytes<'a> {
    inner: &'a [u8],
    /// If truncated, how many bytes were truncated.
    truncated: Option<NonZeroUsize>,
}

impl<'a> From<&'a [u8]> for UnknownBytes<'a> {
    fn from(value: &'a [u8]) -> Self {
        /// Store at most 16 bytes.
        const MAX_BYTES: usize = 16;

        let inner = value.get(..MAX_BYTES).unwrap_or(value);
        let truncated = NonZeroUsize::new(value.len().saturating_sub(MAX_BYTES));

        Self { inner, truncated }
    }
}

impl From<UnknownBytes<'_>> for Value {
    fn from(value: UnknownBytes<'_>) -> Self {
        let values = value.inner.iter().map(|b| format!("{b:#04x}"));
        let truncation_notice = value.truncated.into_iter().map(|count| {
            format!(
                "... ({count} byte{} truncated) ...",
                if count.get() > 1 { "s" } else { "" }
            )
        });

        values.chain(truncation_notice).collect()
    }
}
