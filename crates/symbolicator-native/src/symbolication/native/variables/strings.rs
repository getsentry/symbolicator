//! Detection and decoding of NUL-terminated strings that character pointers point to.

use std::str;

use serde_json::Value as JsonValue;
use symbolic::symcache::{PrimitiveTypeEncoding, Size, Type};

/// The maximum number of code units read for a NUL-terminated string.
///
/// This value was chosen arbitrarily. A string without a NUL terminator in the code units read is
/// reported as truncated.
const MAX_CODE_UNITS: usize = 512;

/// The encoding of a NUL-terminated string, which follows from the type of its characters.
#[derive(Debug, Clone, Copy)]
pub enum StringEncoding {
    /// A string of one-byte characters (`char`, `signed char`, or `unsigned char`), decoded as
    /// UTF-8.
    Utf8,
}

impl StringEncoding {
    /// Returns the encoding of a string whose characters have type `char_type`.
    ///
    /// Returns `None` if `char_type` is not a character type.
    pub fn from_char_type(char_type: &Type<'_>) -> Option<Self> {
        let Type::Primitive(char_type) = char_type else {
            return None;
        };

        let is_byte_char = matches!(char_type.size(), Size::Bytes(1) | Size::Bits(8))
            && matches!(
                char_type.encoding(),
                Some(PrimitiveTypeEncoding::SignedChar | PrimitiveTypeEncoding::UnsignedChar)
            );

        is_byte_char.then_some(Self::Utf8)
    }

    /// The maximum number of bytes to read for a string in this encoding.
    pub fn max_bytes(self) -> usize {
        match self {
            Self::Utf8 => MAX_CODE_UNITS,
        }
    }
}

/// A string read from memory up to its NUL terminator.
pub struct CStringValue<'a> {
    inner: &'a str,
    /// Whether the bytes read contain no NUL terminator.
    truncated: bool,
}

impl<'a> CStringValue<'a> {
    /// Parses a string in `encoding` from `bytes`, which start at the string's address.
    ///
    /// Returns `None` if the code units before the NUL terminator are not entirely valid in
    /// `encoding`. If `bytes` contains no NUL terminator, we attempt to parse the bytes, ignoring
    /// any truncated character at the end, and indicate the string as truncated.
    pub fn parse(bytes: &'a [u8], encoding: StringEncoding) -> Option<Self> {
        match encoding {
            StringEncoding::Utf8 => Self::parse_utf8(bytes),
        }
    }

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

        Some(Self { inner, truncated })
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

    fn parse(bytes: &[u8]) -> Option<(&str, bool)> {
        CStringValue::parse(bytes, StringEncoding::Utf8).map(|s| (s.inner, s.truncated))
    }

    #[test]
    fn test_c_string_terminated() {
        assert_eq!(parse(b"hello\0wo"), Some(("hello", false)));
    }

    #[test]
    fn test_c_string_invalid_utf8() {
        assert_eq!(parse(b"ab\xffcd\0"), None);
    }

    #[test]
    fn test_c_string_incomplete_character() {
        // An incomplete character is only dropped from a truncated string.
        assert_eq!(parse(b"ab\xc3\0"), None);
        assert_eq!(parse(b"ab\xe2\x82"), Some(("ab", true)));
    }
}
