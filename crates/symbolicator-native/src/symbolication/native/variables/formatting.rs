//! Formatting of variable values based on their types.

use std::num::NonZeroUsize;

use serde_json::Value;
use symbolic::symcache::{PointerType, PrimitiveType, PrimitiveTypeEncoding, Type, TypeSize};

use crate::interface::VariableValue;
use crate::memory::MemoryAccess;
use crate::symbolication::native::variables::AddrOrValue;

/// Formats `value` according to `ty`.
///
/// This is a shorthand for creating a [`TypeFormatter`] and calling [`TypeFormatter::format`].
pub fn format(
    memory: &dyn MemoryAccess,
    value: AddrOrValue,
    ty: Type<'_>,
) -> Option<VariableValue> {
    TypeFormatter { memory, value }.format(ty)
}

/// Formats a value from memory or a register according to its type.
struct TypeFormatter<'a> {
    memory: &'a dyn MemoryAccess,
    value: AddrOrValue,
}

impl TypeFormatter<'_> {
    fn format(&self, ty: Type<'_>) -> Option<VariableValue> {
        match ty {
            Type::Pointer(ty) => self.format_pointer(ty),
            Type::Primitive(ty) => self.format_primitive(ty),
            _ => None,
        }
    }

    fn format_pointer(&self, ty: PointerType) -> Option<VariableValue> {
        let TypeSize::Bytes(size) = ty.size();

        match size {
            // Pointers map to `pointer_address`. We assume only 32-bit and 64-bit pointer addresses
            // exist. We can of course support more if needed.
            4 => self
                .value
                .get_value::<u32>(self.memory)
                .map(|v| VariableValue::new().pointer_address(v)),
            8 => self
                .value
                .get_value::<u64>(self.memory)
                .map(|v| VariableValue::new().pointer_address(v)),
            _ => None,
        }
    }

    fn format_primitive(&self, ty: PrimitiveType<'_>) -> Option<VariableValue> {
        use PrimitiveTypeEncoding::*;

        /// Helper to convert the memory at addr to the provided type, and map this to a VariableValue.
        macro_rules! get_value {
            // If we just get a type, map this to a formatted variable value.
            ($ty:ty) => {
                get_value!($ty, |v| VariableValue::new().value(v))
            };

            // Also allow a custom mapping from value to VariableValue, as not everything will map
            // directly.
            ($ty:ty, $($mapping:tt)*) => {
                self.value.get_value::<$ty>(self.memory).map($($mapping)*)
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

        let TypeSize::Bytes(size) = ty.size();

        match (ty.encoding(), size) {
            // Not sure this is realistic, but size 0 probably should map to an empty value.
            // Returning `None` could also make sense.
            (_, 0) => Some(VariableValue::new()),

            // Booleans and Unsigned ints.
            //
            // These need to be handled together because bool cannot be read directly from memory; we
            // instead read the Unsigned integer type and compare against 0 when this is a Boolean.
            (Some(encoding @ (Boolean | UnsignedInt)), 1) => {
                get_value!(u8, map_bool_unsigned!(encoding))
            }
            // Characters more than one byte are handled as integers.
            (Some(encoding @ (Boolean | UnsignedInt | UnsignedChar)), 2) => {
                get_value!(u16, map_bool_unsigned!(encoding))
            }
            (Some(encoding @ (Boolean | UnsignedInt | UnsignedChar)), 4) => {
                get_value!(u32, map_bool_unsigned!(encoding))
            }
            (Some(encoding @ (Boolean | UnsignedInt | UnsignedChar)), 8) => {
                get_value!(u64, map_bool_unsigned!(encoding))
            }
            (Some(encoding @ (Boolean | UnsignedInt | UnsignedChar)), 16) => {
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
            (Some(SignedInt), 1) => get_value!(i8),
            (Some(SignedInt | SignedChar), 2) => get_value!(i16),
            (Some(SignedInt | SignedChar), 4) => get_value!(i32),
            (Some(SignedInt | SignedChar), 8) => get_value!(i64),
            (Some(SignedInt | SignedChar), 16) => {
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
            (Some(Float), 4) => get_value!(f32),
            (Some(Float), 8) => get_value!(f64),

            // 1-byte characters. Multibyte characters handled as integers.
            (Some(SignedChar | UnsignedChar), 1) => {
                get_value!(u8, |v| format_variable!("'{}'", v.escape_ascii()))
            }

            // Handle memory addresses by rending to a hexadecimal string.
            (Some(Address), 1) => get_value!(u8, |v| format_variable!("{v:#04x}")),
            (Some(Address), 2) => get_value!(u16, |v| format_variable!("{v:#06x}")),
            (Some(Address), 4) => get_value!(u32, |v| format_variable!("{v:#010x}")),
            (Some(Address), 8) => get_value!(u64, |v| format_variable!("{v:#018x}")),
            (Some(Address), 16) => get_value!(u128, |v| format_variable!("{v:#034x}")),

            // Unknown/unhandled data types.
            // We limit the size of memory slice we extract to 16 via UnknownBytes.
            (_, size) => size
                .try_into()
                .ok()
                .and_then(|size| self.value.get_memory(self.memory, size))
                .map(|v| VariableValue::new().value(UnknownBytes::from(v))),
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
