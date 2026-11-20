//! Detection and decoding of NUL-terminated strings that character pointers point to.

use std::borrow::Cow;
use std::str;

use serde_json::Value as JsonValue;
use symbolic::symcache::{PrimitiveType, PrimitiveTypeEncoding, Size, SymCache, Type};

use crate::memory::Endian;

/// The maximum number of code units read for a NUL-terminated string.
///
/// This value was chosen arbitrarily. A string without a NUL terminator in the code units read is
/// reported as truncated.
const MAX_CODE_UNITS: usize = 512;

/// The name of the wide character type, a base type in C++ and a typedef of an integer type in C.
const WCHAR_T: &str = "wchar_t";

/// The encoding of a NUL-terminated string, which follows from the type of its characters.
#[derive(Debug, Clone, Copy)]
pub enum StringEncoding {
    /// A string of one-byte characters (`char`, `signed char`, or `unsigned char`), decoded as
    /// UTF-8.
    Utf8,
    /// A string of two-byte `wchar_t` characters, decoded as UTF-16 in the crash's byte order.
    ///
    /// Windows uses a two-byte `wchar_t`.
    Utf16,
    /// A string of four-byte `wchar_t` characters, decoded as UTF-32 in the crash's byte order.
    ///
    /// Linux, macOS, and Android use a four-byte `wchar_t`.
    Utf32,
}

impl StringEncoding {
    /// Returns the encoding of a string whose characters have type `char_type`.
    ///
    /// Returns `None` if `char_type` is not a character type.
    pub fn from_char_type(cache: &SymCache<'_>, char_type: &Type<'_>) -> Option<Self> {
        match char_type {
            Type::Primitive(char_type) if char_type.name() == Some(WCHAR_T) => {
                Self::from_wchar_t(char_type)
            }
            Type::Primitive(char_type) => {
                let is_byte_char = matches!(char_type.size(), Size::Bytes(1) | Size::Bits(8))
                    && matches!(
                        char_type.encoding(),
                        Some(
                            PrimitiveTypeEncoding::SignedChar | PrimitiveTypeEncoding::UnsignedChar
                        )
                    );

                is_byte_char.then_some(Self::Utf8)
            }
            Type::TypeDef(char_type) if char_type.name() == WCHAR_T => {
                match cache.lookup_type(char_type.ty()?)? {
                    Type::Primitive(target) => Self::from_wchar_t(&target),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Returns the encoding of a `wchar_t` string, which follows from the size of `wchar_t`.
    fn from_wchar_t(wchar_t: &PrimitiveType<'_>) -> Option<Self> {
        match wchar_t.size() {
            Size::Bytes(2) | Size::Bits(16) => Some(Self::Utf16),
            Size::Bytes(4) | Size::Bits(32) => Some(Self::Utf32),
            _ => None,
        }
    }

    /// The maximum number of bytes to read for a string in this encoding.
    pub fn max_bytes(self) -> usize {
        let code_unit_size = match self {
            Self::Utf8 => 1,
            Self::Utf16 => 2,
            Self::Utf32 => 4,
        };

        MAX_CODE_UNITS * code_unit_size
    }
}

/// A string read from memory up to its NUL terminator.
pub struct CStringValue<'a> {
    inner: Cow<'a, str>,
    /// Whether the bytes read contain no NUL terminator.
    truncated: bool,
}

impl<'a> CStringValue<'a> {
    /// Parses a string in `encoding` from `bytes`, which start at the string's address.
    ///
    /// Multi-byte code units are read in `endian` byte order.
    ///
    /// Returns `None` if the code units before the NUL terminator are not entirely valid in
    /// `encoding`. If `bytes` contains no NUL terminator, we attempt to parse the bytes, ignoring
    /// any truncated character at the end, and indicate the string as truncated.
    pub fn parse(bytes: &'a [u8], encoding: StringEncoding, endian: Endian) -> Option<Self> {
        match encoding {
            StringEncoding::Utf8 => Self::parse_utf8(bytes),
            StringEncoding::Utf16 => Self::parse_utf16(&code_units(
                bytes,
                endian,
                u16::from_le_bytes,
                u16::from_be_bytes,
            )),
            StringEncoding::Utf32 => Self::parse_utf32(&code_units(
                bytes,
                endian,
                u32::from_le_bytes,
                u32::from_be_bytes,
            )),
        }
    }

    fn parse_utf8(bytes: &'a [u8]) -> Option<Self> {
        let (bytes, truncated) = until_nul(bytes);

        let inner = match str::from_utf8(bytes) {
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

    fn parse_utf16(units: &[u16]) -> Option<Self> {
        let (mut units, truncated) = until_nul(units);

        // A truncated string may end between the two halves of a surrogate pair.
        if truncated && let [rest @ .., 0xD800..=0xDBFF] = units {
            units = rest;
        }

        let inner = String::from_utf16(units).ok()?;
        Some(Self {
            inner: inner.into(),
            truncated,
        })
    }

    fn parse_utf32(units: &[u32]) -> Option<Self> {
        let (units, truncated) = until_nul(units);

        let inner = units
            .iter()
            .map(|&unit| char::from_u32(unit))
            .collect::<Option<String>>()?;
        Some(Self {
            inner: inner.into(),
            truncated,
        })
    }
}

/// Reads `bytes` as code units of `N` bytes in `endian` byte order.
///
/// Trailing bytes that do not form a whole code unit are ignored.
fn code_units<const N: usize, T>(
    bytes: &[u8],
    endian: Endian,
    from_le_bytes: fn([u8; N]) -> T,
    from_be_bytes: fn([u8; N]) -> T,
) -> Vec<T> {
    let from_bytes = match endian {
        Endian::Little => from_le_bytes,
        Endian::Big => from_be_bytes,
    };

    let (units, _) = bytes.as_chunks::<N>();
    units.iter().map(|&unit| from_bytes(unit)).collect()
}

/// Returns the code units before the first NUL, and whether `units` contains no NUL.
fn until_nul<T: Default + PartialEq>(units: &[T]) -> (&[T], bool) {
    match units.iter().position(|unit| *unit == T::default()) {
        Some(end) => (&units[..end], false),
        None => (units, true),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Option<(String, bool)> {
        parse_with(bytes, StringEncoding::Utf8, Endian::Little)
    }

    fn parse_with(
        bytes: &[u8],
        encoding: StringEncoding,
        endian: Endian,
    ) -> Option<(String, bool)> {
        CStringValue::parse(bytes, encoding, endian).map(|s| (s.inner.into_owned(), s.truncated))
    }

    fn utf16_le(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
    }

    #[test]
    fn test_c_string_terminated() {
        assert_eq!(parse(b"hello\0wo"), Some(("hello".into(), false)));
    }

    #[test]
    fn test_c_string_invalid_utf8() {
        assert_eq!(parse(b"ab\xffcd\0"), None);
    }

    #[test]
    fn test_c_string_incomplete_character() {
        // An incomplete character is only dropped from a truncated string.
        assert_eq!(parse(b"ab\xc3\0"), None);
        assert_eq!(parse(b"ab\xe2\x82"), Some(("ab".into(), true)));
    }

    #[test]
    fn test_utf16_string_with_surrogate_pair() {
        // "a😀" followed by a NUL and a unit that is not part of the string.
        let bytes = utf16_le(&[0x61, 0xD83D, 0xDE00, 0, 0x62]);
        assert_eq!(
            parse_with(&bytes, StringEncoding::Utf16, Endian::Little),
            Some(("a😀".into(), false))
        );
    }

    #[test]
    fn test_utf16_string_big_endian() {
        assert_eq!(
            parse_with(b"\0h\0i\0\0", StringEncoding::Utf16, Endian::Big),
            Some(("hi".into(), false))
        );
    }

    #[test]
    fn test_utf16_string_unpaired_surrogate() {
        let bytes = utf16_le(&[0x61, 0xDE00, 0x62, 0]);
        assert_eq!(
            parse_with(&bytes, StringEncoding::Utf16, Endian::Little),
            None
        );
    }

    #[test]
    fn test_utf16_string_truncated_surrogate_pair() {
        // A high surrogate is only dropped from a truncated string.
        let bytes = utf16_le(&[0x61, 0xD83D, 0]);
        assert_eq!(
            parse_with(&bytes, StringEncoding::Utf16, Endian::Little),
            None
        );

        // The trailing odd byte does not form a code unit.
        let mut bytes = utf16_le(&[0x61, 0xD83D]);
        bytes.push(0xDE);
        assert_eq!(
            parse_with(&bytes, StringEncoding::Utf16, Endian::Little),
            Some(("a".into(), true))
        );
    }

    #[test]
    fn test_utf32_string() {
        let bytes: Vec<u8> = [0x61, 0x1F600, 0, 0x62]
            .iter()
            .flat_map(|unit: &u32| unit.to_le_bytes())
            .collect();
        assert_eq!(
            parse_with(&bytes, StringEncoding::Utf32, Endian::Little),
            Some(("a😀".into(), false))
        );
    }

    #[test]
    fn test_utf32_string_invalid_scalar_value() {
        // A surrogate is not a valid Unicode scalar value.
        let bytes: Vec<u8> = [0x61, 0xD83D, 0]
            .iter()
            .flat_map(|unit: &u32| unit.to_le_bytes())
            .collect();
        assert_eq!(
            parse_with(&bytes, StringEncoding::Utf32, Endian::Little),
            None
        );
    }
}
