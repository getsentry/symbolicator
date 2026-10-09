//! Module containing logic for extracting variable values from native crashes.

use minidump::CpuContext;
use scroll::Endian;
use serde_json::Value;
use symbolic::common::CpuFamily;
use symbolic::symcache::{
    PrimitiveTypeEncoding, SourceLocation, SymCache, Type, TypeSize, VariableLocation,
    VariableLocationInfo,
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

            let value = variable.locations().find_map(|loc| {
                resolve_variable_value(cache, registers, memory, loc, variable.ty())
            })?;

            let interface_variable = value.ty(ty).kind(variable.kind());

            Some((name, interface_variable))
        })
        .collect()
}

fn resolve_variable_value(
    cache: &SymCache<'_>,
    registers: &Registers,
    memory: &dyn MemoryAccess,
    location: VariableLocationInfo,
    ty: Option<Type<'_>>,
) -> Option<VariableValue> {
    let ty = ty?;

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
            .copied()
            .map(RegisterMemoryAccess::from_hex_value)
            .and_then(|reg_mem| format_value(&reg_mem, 0, ty))
        }
        VariableLocation::FrameOffset { offset } => {
            let &HexValue(frame_base) = match cache.arch().cpu_family() {
                CpuFamily::Amd64 => Some("rbp"),
                CpuFamily::Arm64 => Some("fp"),
                _ => None,
            }
            .and_then(|reg| registers.get(reg))?;

            let addr = u64::try_from(i64::try_from(frame_base).ok()? + offset).ok()?;

            format_value(memory, addr, ty)
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

fn format_value(memory: &dyn MemoryAccess, addr: u64, ty: Type<'_>) -> Option<VariableValue> {
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
            memory.get_value_at_address::<$ty>(addr).map($($mapping)*)
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

    // Actual beginning of the function is here, everything above are helper type and macro defs.

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

        // TODO: Is it correct to extract both signed and unsigned chars as u8 in this way?
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
        //
        // We pass these through `get_value!` to `get_value_at_address` rather than going through
        // `get_memory_at_address`, as this allows the bytes to be rendered according to their
        // endianness on the target system. Doing this requires us to match each separate constant
        // size specifically.
        (_, 1) => get_value!([u8; 1], map_raw_bytes_variable_value),
        (_, 2) => get_value!([u8; 2], map_raw_bytes_variable_value),
        (_, 3) => get_value!([u8; 3], map_raw_bytes_variable_value),
        (_, 4) => get_value!([u8; 4], map_raw_bytes_variable_value),
        (_, 5) => get_value!([u8; 5], map_raw_bytes_variable_value),
        (_, 6) => get_value!([u8; 6], map_raw_bytes_variable_value),
        (_, 7) => get_value!([u8; 7], map_raw_bytes_variable_value),
        (_, 8) => get_value!([u8; 8], map_raw_bytes_variable_value),
        (_, 9) => get_value!([u8; 9], map_raw_bytes_variable_value),
        (_, 10) => get_value!([u8; 10], map_raw_bytes_variable_value),
        (_, 11) => get_value!([u8; 11], map_raw_bytes_variable_value),
        (_, 12) => get_value!([u8; 12], map_raw_bytes_variable_value),
        (_, 13) => get_value!([u8; 13], map_raw_bytes_variable_value),
        (_, 14) => get_value!([u8; 14], map_raw_bytes_variable_value),
        (_, 15) => get_value!([u8; 15], map_raw_bytes_variable_value),
        (_, 16) => get_value!([u8; 16], map_raw_bytes_variable_value),

        // We limit the size of memory slice we extract to 16.
        //
        // Memory slices with size ≤16 are already handled above. Here, we handle bigger slices by
        // extracting the sixteen bytes at the address. The rendered byte array will contain a
        // truncation notice in the appropriate position depending on the endianness of the memory.
        (_, size) => get_value!([u8; 16], |v| {
            let bytes = RawBytes::from(v).truncated(size - 16, memory.endian());
            VariableValue::new().value(bytes)
        }),
    }
}

#[derive(Debug)]
struct RegisterMemoryAccess<const N: usize> {
    memory: [u8; N],
}

impl RegisterMemoryAccess<8> {
    fn from_hex_value(value: HexValue) -> Self {
        let memory = value.0.to_le_bytes();
        Self { memory }
    }
}

impl<const N: usize> MemoryAccess for RegisterMemoryAccess<N> {
    fn get_memory_at_address(&self, addr: u64, size: usize) -> Option<&'_ [u8]> {
        let start: usize = addr.try_into().ok()?;
        let end = start.checked_add(size)?;

        self.memory.get(start..end)
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }
}

/// An array of bytes without further meaning.
///
/// This provides a conversion into [`serde_json::Value`]. That conversion maps to an array, where
/// each element is a string containing the hexadecimal represenation of the byte. It also handles
/// including information about an amount of values truncated, and the position is according to
/// the data's endianness.
struct RawBytes<const N: usize> {
    inner: [u8; N],
    /// Whether we truncated the bytes, and if so, what endianness the data has, and how many bytes
    /// we truncated.
    truncated: Option<(u64, Endian)>,
}

impl<const N: usize> From<[u8; N]> for RawBytes<N> {
    fn from(value: [u8; N]) -> Self {
        Self {
            inner: value,
            truncated: None,
        }
    }
}

impl<const N: usize> RawBytes<N> {
    fn truncated(mut self, count: u64, endian: Endian) -> Self {
        self.truncated = Some((count, endian));
        self
    }
}

impl<const N: usize> From<RawBytes<N>> for Value {
    fn from(value: RawBytes<N>) -> Self {
        let truncation_notice = |count| {
            format!(
                "... ({count} byte{} truncated) ...",
                if count > 1 { "s" } else { "" }
            )
        };

        let values = value.inner.into_iter().map(|b| format!("{b:#04x}"));
        let (start_truncation_notice, end_truncation_notice) = match value.truncated {
            Some((count, Endian::Little)) => (Some(truncation_notice(count)), None),
            Some((count, Endian::Big)) => (None, Some(truncation_notice(count))),
            None => (None, None),
        };

        start_truncation_notice
            .into_iter()
            .chain(values)
            .chain(end_truncation_notice)
            .collect()
    }
}

/// Helper to map a byte array to a [`RawBytes`] [`VariableValue`].
fn map_raw_bytes_variable_value<const N: usize>(value: [u8; N]) -> VariableValue {
    VariableValue::new().value(RawBytes::from(value))
}
