//! Formatting of variable values based on their types.

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::str;

use serde_json::Value as JsonValue;
use symbolic::debuginfo::ValueBits;
use symbolic::symcache::{
    PointerType, PrimitiveType, PrimitiveTypeEncoding, Size, StructureType, SymCache, Type,
    TypeDefType,
};

use crate::interface::{PointerAddress, VariableValue};
use crate::memory::{MemoryAccess, MemoryAccessExt, MemoryView};
use crate::symbolication::native::variables::AddrOrValue;

/// The absolute depth limit for type traversal.
const TYPE_FORMAT_DEPTH_LIMIT: usize = 10;

/// Formats `value` according to `ty`.
///
/// This is a shorthand for creating a [`TypeFormatter`] and calling [`TypeFormatter::format`].
pub fn format(
    cache: &SymCache<'_>,
    memory: &dyn MemoryAccess,
    value: AddrOrValue<'_>,
    ty: Type<'_>,
) -> Option<VariableValue> {
    TypeFormatter {
        cache,
        memory,
        remaining_depth: TYPE_FORMAT_DEPTH_LIMIT,
    }
    .format(value, ty)
}

/// Formats a value from memory or a register according to its type.
struct TypeFormatter<'a> {
    cache: &'a SymCache<'a>,
    memory: &'a dyn MemoryAccess,
    remaining_depth: usize,
}

impl TypeFormatter<'_> {
    fn format(&mut self, value: AddrOrValue<'_>, ty: Type<'_>) -> Option<VariableValue> {
        self.remaining_depth = self.remaining_depth.checked_sub(1)?;

        let result = match ty {
            Type::Pointer(ty) => self.format_pointer(value, ty),
            Type::Primitive(ty) => self.format_primitive(value, ty),
            Type::TypeDef(ty) => self.format_typedef(value, ty),
            Type::StructureType(ty) => self.format_structure(value, ty),
            _ => None,
        };

        self.remaining_depth += 1;
        result
    }

    fn format_pointer(&mut self, value: AddrOrValue<'_>, ty: PointerType) -> Option<VariableValue> {
        let addr = self.read_pointer(value, &ty)?;

        // Currently multi-level pointers directly resolve to their final value, if de-referencing
        // fails on the way the final value/pointee is just not set. We could be doing better here
        // and serializing the entire chain. Either always or only if we can't resolve the final value.

        let mut lookups = 0;
        let mut pointee = ty.pointee();
        let mut pointee_addr = dereferenceable(addr.into());
        let (base_name, pointee) = loop {
            lookups += 1;

            let Some(pointee_id) = pointee else {
                break ("void", None);
            };

            if lookups > self.remaining_depth {
                break ("<unknown>", None);
            }

            match self.cache.lookup_type(pointee_id) {
                Some(Type::Pointer(ptr)) => {
                    pointee = ptr.pointee();
                    pointee_addr = pointee_addr
                        .and_then(|addr| self.read_pointer(AddrOrValue::Addr(addr), &ptr))
                        .map(u64::from)
                        .and_then(dereferenceable);
                }
                Some(ty) => break (type_name(&ty), Some(ty)),
                None => break ("<unknown>", None),
            }
        };

        let name = format!("{base_name}{:*<lookups$}", "");
        let mut result = VariableValue::new(name).pointer_address(addr);

        // Attempt to parse only single-level pointers as strings. A string becomes the pointer's
        // value instead of its pointee.
        if lookups == 1
            && let Some((pointee_type, pointee_addr)) = pointee.as_ref().zip(pointee_addr)
            && let Some(width) = self.char_width(pointee_type)
            && let Some(string) = self.format_c_string(pointee_addr, width)
        {
            return Some(result.value(string));
        }

        let pointee = pointee
            .zip(pointee_addr)
            .and_then(|(ty, addr)| self.format(AddrOrValue::Addr(addr), ty));
        if let Some(pointee) = pointee {
            result = result.pointee(pointee);
        }

        Some(result)
    }

    fn format_primitive(
        &self,
        value: AddrOrValue<'_>,
        ty: PrimitiveType<'_>,
    ) -> Option<VariableValue> {
        let name = ty.name().unwrap_or("<unknown>");
        let result = VariableValue::new(name);

        let ValueBits { size, offset } = ty.value_bits().unwrap_or_else(|| {
            let size = match ty.size() {
                Size::Bytes(bytes) => bytes * 8,
                Size::Bits(bits) => bits,
            };
            ValueBits { size, offset: 0 }
        });

        let Some(bits) = value.get_bits(self.memory, offset, size) else {
            let value = size
                .is_multiple_of(8)
                .then_some(size)
                .and_then(|size| value.get_memory(self.memory, (size / 8) as usize))
                .map(UnknownBytes::from);

            return Some(result.value_opt(value));
        };

        use PrimitiveTypeEncoding::*;
        let formatted: JsonValue = match (ty.encoding(), size) {
            (Some(Boolean), _) => bits.as_bool().into(),
            (Some(SignedChar | UnsignedChar), 8) => {
                format!("'{}'", (bits.as_u128() as u8).escape_ascii()).into()
            }
            (Some(SignedInt | SignedChar), _) => match i64::try_from(bits.as_i128()) {
                Ok(int) => int.into(),
                Err(_) => bits.as_i128().to_string().into(),
            },
            (Some(UnsignedInt | UnsignedChar), _) => match u64::try_from(bits.as_u128()) {
                Ok(int) => int.into(),
                Err(_) => bits.as_u128().to_string().into(),
            },
            (Some(Float), 32) => f32::from_bits(bits.as_u128() as u32).into(),
            (Some(Float), 64) => f64::from_bits(bits.as_u128() as u64).into(),
            (Some(Address), _) => {
                let address = bits.as_u128();
                let width = size.div_ceil(4) as usize + 2;
                format!("{address:#0width$x}").into()
            }
            (Some(Float | ComplexFloat) | None, _) => {
                let data = bits.as_u128();
                let width = size as usize;
                format!("0b{data:0width$b}").into()
            }
        };

        Some(result.value(formatted))
    }

    fn format_typedef(
        &mut self,
        value: AddrOrValue<'_>,
        ty: TypeDefType<'_>,
    ) -> Option<VariableValue> {
        let name = ty.name();
        let Some(target) = ty.ty().and_then(|ty| self.cache.lookup_type(ty)) else {
            return Some(VariableValue::new(name));
        };

        self.format(value, target).map(|value| value.ty(name))
    }

    fn format_structure(
        &mut self,
        value: AddrOrValue<'_>,
        ty: StructureType<'_>,
    ) -> Option<VariableValue> {
        let mut result = VariableValue::new(ty.name().unwrap_or("<anonymous>"));
        for member in ty.members() {
            let ty = self
                .cache
                .lookup_type(member.ty())
                .and_then(|ty| {
                    let offset = match member.offset()? {
                        Size::Bytes(offset) => offset,
                        // Currently not supported.
                        Size::Bits(_) => return None,
                    };
                    self.format(value.offset(offset), ty)
                })
                .unwrap_or_default();

            result = result.member(member.name().unwrap_or("<anonymous>"), ty);
        }

        Some(result)
    }

    /// Returns the width of `ty` if it is a character type of a NUL-terminated string.
    ///
    /// DWARF encodes `wchar_t` as a plain integer, so it is detected by name: a base type in C++,
    /// a typedef in C.
    fn char_width(&self, ty: &Type<'_>) -> Option<CharWidth> {
        match ty {
            Type::Primitive(ty) if ty.name() == Some("wchar_t") => CharWidth::wide(ty.size()),
            Type::Primitive(ty) => {
                let is_char = matches!(ty.size(), Size::Bytes(1) | Size::Bits(8))
                    && matches!(
                        ty.encoding(),
                        Some(
                            PrimitiveTypeEncoding::SignedChar | PrimitiveTypeEncoding::UnsignedChar
                        )
                    );
                is_char.then_some(CharWidth::U8)
            }
            Type::TypeDef(ty) if ty.name() == "wchar_t" => {
                match self.cache.lookup_type(ty.ty()?)? {
                    Type::Primitive(target) => CharWidth::wide(target.size()),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Formats the NUL-terminated string at `addr` with characters of the given `width`.
    ///
    /// Returns `None` if the memory at `addr` is not a valid string.
    fn format_c_string(&self, addr: u64, width: CharWidth) -> Option<JsonValue> {
        /// The maximum number of characters read for a NUL-terminated string.
        ///
        /// This value was chosen arbitrarily. A string without a NUL terminator in the characters
        /// read is reported as truncated.
        const MAX_UNITS: usize = 512;

        let string = match width {
            CharWidth::U8 => {
                CStringValue::parse_utf8(self.memory.get_memory_up_to(addr, MAX_UNITS)?)
            }
            CharWidth::U16 => {
                CStringValue::parse_utf16(self.memory.get_values_up_to(addr, MAX_UNITS)?)
            }
            CharWidth::U32 => {
                CStringValue::parse_utf32(self.memory.get_values_up_to(addr, MAX_UNITS)?)
            }
        };

        string.map(JsonValue::from)
    }

    /// Reads the address stored in the pointer `ty` located at `value`.
    fn read_pointer(&self, value: AddrOrValue<'_>, ty: &PointerType) -> Option<PointerAddress> {
        match ty.size() {
            // Pointers map to `pointer_address`. We assume only 32-bit and 64-bit pointer addresses
            // exist. We can of course support more if needed.
            Size::Bytes(4) | Size::Bits(32) => value.get_value::<u32>(self.memory).map(Into::into),
            Size::Bytes(8) | Size::Bits(64) => value.get_value::<u64>(self.memory).map(Into::into),
            _ => None,
        }
    }
}

/// Returns `addr` if it can be dereferenced.
fn dereferenceable(addr: u64) -> Option<u64> {
    /// Size of the first page, which is never mapped.
    const FIRST_PAGE: u64 = 0x1000;

    // There are more architecture specific rules we can implement here, these are implemented in
    // the minidump crate.

    (addr >= FIRST_PAGE).then_some(addr)
}

/// Returns the name of a type.
///
/// This returns the name contained on the [`Type`] and does not recurse to construct a name.
fn type_name<'data>(ty: &Type<'data>) -> &'data str {
    match ty {
        Type::Primitive(ty) => ty.name().unwrap_or("<unknown>"),
        Type::TypeDef(ty) => ty.name(),
        Type::StructureType(ty) => ty.name().unwrap_or("<anonymous>"),
        _ => "<not implemented>",
    }
}

/// The width of the characters in a NUL-terminated string.
#[derive(Clone, Copy)]
enum CharWidth {
    /// `char`, UTF-8.
    U8,
    /// 2-byte `wchar_t`, UTF-16.
    U16,
    /// 4-byte `wchar_t`, UTF-32.
    U32,
}

impl CharWidth {
    /// Returns the width of a `wchar_t` of the given size.
    fn wide(size: Size) -> Option<Self> {
        match size {
            Size::Bytes(2) | Size::Bits(16) => Some(Self::U16),
            Size::Bytes(4) | Size::Bits(32) => Some(Self::U32),
            _ => None,
        }
    }
}

/// A string read from memory up to its NUL terminator.
struct CStringValue<'a> {
    inner: Cow<'a, str>,
    /// Whether the characters read contain no NUL terminator.
    truncated: bool,
}

impl<'a> CStringValue<'a> {
    /// Parses a UTF-8 string from `bytes`, which start at the string's address.
    ///
    /// Returns `None` if the bytes before the NUL terminator are not entirely valid UTF-8. If
    /// `bytes` contains no NUL terminator, we attempt to parse the bytes as UTF-8, ignoring any
    /// truncated multibyte character at the end, and indicate the string as truncated.
    fn parse_utf8(bytes: &'a [u8]) -> Option<Self> {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let truncated = end == bytes.len();

        let inner = match str::from_utf8(&bytes[..end]) {
            Ok(inner) => inner,
            // `error_len` is `None` if the error is an incomplete character at the end of `bytes`.
            Err(error) if truncated && error.error_len().is_none() => {
                str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
            }
            Err(_) => return None,
        };

        Some(Self {
            inner: inner.into(),
            truncated,
        })
    }

    /// Parses a UTF-16 string from `units`, which start at the string's address.
    ///
    /// Like [`Self::parse_utf8`], this returns `None` for invalid UTF-16 before the NUL terminator, and
    /// ignores an unpaired high surrogate at the end of a truncated string.
    fn parse_utf16(units: MemoryView<'_, u16>) -> Option<Self> {
        let end = units.iter().position(|unit| unit == 0);
        let truncated = end.is_none();

        let mut chars = char::decode_utf16(units.iter().take(end.unwrap_or(usize::MAX))).peekable();
        let mut inner = String::new();
        while let Some(char) = chars.next() {
            match char {
                Ok(char) => inner.push(char),
                // A high surrogate at the end of a truncated string may be the start of a
                // character that was cut off.
                Err(error)
                    if truncated
                        && chars.peek().is_none()
                        && (0xd800..0xdc00).contains(&error.unpaired_surrogate()) => {}
                Err(_) => return None,
            }
        }

        Some(Self {
            inner: inner.into(),
            truncated,
        })
    }

    /// Parses a UTF-32 string from `units`, which start at the string's address.
    ///
    /// Returns `None` if a unit before the NUL terminator is not a valid character.
    fn parse_utf32(units: MemoryView<'_, u32>) -> Option<Self> {
        let end = units.iter().position(|unit| unit == 0);
        let inner = units
            .iter()
            .take(end.unwrap_or(usize::MAX))
            .map(char::from_u32)
            .collect::<Option<String>>()?;

        Some(Self {
            inner: inner.into(),
            truncated: end.is_none(),
        })
    }
}

impl From<CStringValue<'_>> for JsonValue {
    fn from(value: CStringValue<'_>) -> Self {
        if value.truncated {
            format!("{}... (truncated)", value.inner).into()
        } else {
            value.inner.into()
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

impl From<UnknownBytes<'_>> for JsonValue {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Endian, SliceMemory};

    fn parse_utf8(bytes: &[u8]) -> Option<(String, bool)> {
        let string = CStringValue::parse_utf8(bytes)?;
        Some((string.inner.into_owned(), string.truncated))
    }

    fn parse_utf16(units: &[u16]) -> Option<(String, bool)> {
        let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        let memory = SliceMemory::new(&bytes, Endian::Little);
        let string = CStringValue::parse_utf16(memory.get_values_up_to(0, units.len())?)?;
        Some((string.inner.into_owned(), string.truncated))
    }

    fn parse_utf32(units: &[u32]) -> Option<(String, bool)> {
        let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        let memory = SliceMemory::new(&bytes, Endian::Little);
        let string = CStringValue::parse_utf32(memory.get_values_up_to(0, units.len())?)?;
        Some((string.inner.into_owned(), string.truncated))
    }

    #[test]
    fn test_c_string_terminated() {
        assert_eq!(parse_utf8(b"hello\0wo"), Some(("hello".into(), false)));
    }

    #[test]
    fn test_c_string_invalid_utf8() {
        assert_eq!(parse_utf8(b"ab\xffcd\0"), None);
    }

    #[test]
    fn test_c_string_incomplete_character() {
        // An incomplete character is only dropped from a truncated string.
        assert_eq!(parse_utf8(b"ab\xc3\0"), None);
        assert_eq!(parse_utf8(b"ab\xe2\x82"), Some(("ab".into(), true)));
    }

    #[test]
    fn test_utf16_string() {
        // "a😀" followed by the NUL terminator and unrelated memory.
        assert_eq!(
            parse_utf16(&[0x61, 0xd83d, 0xde00, 0, 0x62]),
            Some(("a😀".into(), false))
        );
        assert_eq!(parse_utf16(&[0x61, 0x62]), Some(("ab".into(), true)));
        // An unpaired surrogate is only dropped from the end of a truncated string.
        assert_eq!(parse_utf16(&[0x61, 0xd83d, 0x62, 0]), None);
        assert_eq!(parse_utf16(&[0x61, 0xd83d, 0]), None);
        assert_eq!(parse_utf16(&[0x61, 0xd83d]), Some(("a".into(), true)));
    }

    #[test]
    fn test_utf32_string() {
        assert_eq!(
            parse_utf32(&[0x61, 0x1f600, 0, 0x62]),
            Some(("a😀".into(), false))
        );
        assert_eq!(parse_utf32(&[0x61, 0x62]), Some(("ab".into(), true)));
        assert_eq!(parse_utf32(&[0x61, 0xd83d, 0]), None);
    }
}
