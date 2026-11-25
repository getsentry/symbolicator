//! Formatting of variable values based on their types.

use std::num::NonZeroUsize;
use std::{iter, mem, str};

use serde_json::Value as JsonValue;
use symbolic::debuginfo::ValueBits;
use symbolic::symcache::{
    PointerType, PrimitiveType, PrimitiveTypeEncoding, Qualifiers, Size, StructureType, SymCache,
    Type, TypeDefType,
};

use crate::interface::{PointerAddress, VariableValue};
use crate::memory::MemoryAccess;
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
    .format(value, ty, Qualifiers::empty())
}

/// Formats a value from memory or a register according to its type.
struct TypeFormatter<'a> {
    cache: &'a SymCache<'a>,
    memory: &'a dyn MemoryAccess,
    remaining_depth: usize,
}

impl TypeFormatter<'_> {
    /// Formats `value` according to `ty`, with the `qualifiers` that apply to `ty`.
    fn format(
        &mut self,
        value: AddrOrValue<'_>,
        ty: Type<'_>,
        qualifiers: Qualifiers,
    ) -> Option<VariableValue> {
        self.remaining_depth = self.remaining_depth.checked_sub(1)?;

        let result = match ty {
            Type::Qualified(ty) => self
                .cache
                .lookup_type(ty.ty())
                .and_then(|inner| self.format(value, inner, qualifiers | ty.qualifiers())),
            Type::Pointer(ty) => self.format_pointer(value, ty, qualifiers),
            Type::Primitive(ty) => self.format_primitive(value, ty, qualifiers),
            Type::TypeDef(ty) => self.format_typedef(value, ty, qualifiers),
            Type::StructureType(ty) => self.format_structure(value, ty, qualifiers),
            Type::Void => Some(VariableValue::new(qualified_name(qualifiers, "void"))),
            _ => None,
        };

        self.remaining_depth += 1;
        result
    }

    fn format_pointer(
        &mut self,
        value: AddrOrValue<'_>,
        ty: PointerType,
        qualifiers: Qualifiers,
    ) -> Option<VariableValue> {
        let addr = self.read_pointer(value, &ty)?;

        // Currently multi-level pointers directly resolve to their final value, if de-referencing
        // fails on the way the final value/pointee is just not set. We could be doing better here
        // and serializing the entire chain. Either always or only if we can't resolve the final value.

        let mut lookups = 0;
        // The qualifiers of each pointer level, starting with the outermost pointer.
        let mut pointer_qualifiers = vec![qualifiers];
        // The qualifiers of the current pointee.
        let mut pointee_qualifiers = Qualifiers::empty();
        let mut pointee = ty.pointee();
        let mut pointee_addr = dereferenceable(addr.into());
        let (base_name, pointee) = loop {
            lookups += 1;

            if lookups > self.remaining_depth {
                break ("<unknown>".to_owned(), None);
            }

            let Some(pointee_type) = self.cache.lookup_type(pointee) else {
                break ("<unknown>".to_owned(), None);
            };
            match pointee_type {
                Type::Qualified(qualified) => {
                    pointee_qualifiers |= qualified.qualifiers();
                    pointee = qualified.ty();
                }
                Type::Pointer(ptr) => {
                    pointer_qualifiers.push(mem::take(&mut pointee_qualifiers));
                    pointee = ptr.pointee();
                    pointee_addr = pointee_addr
                        .and_then(|addr| self.read_pointer(AddrOrValue::Addr(addr), &ptr))
                        .map(u64::from)
                        .and_then(dereferenceable);
                }
                ty => break (qualified_name(pointee_qualifiers, type_name(&ty)), Some(ty)),
            }
        };

        let name = pointer_name(&base_name, &pointer_qualifiers);
        let mut result = VariableValue::new(name).pointer_address(addr);

        // Attempt to parse only single-level pointers as strings. A string becomes the pointer's
        // value instead of its pointee.
        if pointer_qualifiers.len() == 1
            && let Some((pointee_type, pointee_addr)) = pointee.as_ref().zip(pointee_addr)
            && let Some(string) = self.format_c_string(pointee_addr, pointee_type)
        {
            return Some(result.value(string));
        }

        let pointee = pointee
            .zip(pointee_addr)
            .and_then(|(ty, addr)| self.format(AddrOrValue::Addr(addr), ty, pointee_qualifiers));
        if let Some(pointee) = pointee {
            result = result.pointee(pointee);
        }

        Some(result)
    }

    fn format_primitive(
        &self,
        value: AddrOrValue<'_>,
        ty: PrimitiveType<'_>,
        qualifiers: Qualifiers,
    ) -> Option<VariableValue> {
        let name = ty.name().unwrap_or("<unknown>");
        let result = VariableValue::new(qualified_name(qualifiers, name));

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
        qualifiers: Qualifiers,
    ) -> Option<VariableValue> {
        let name = qualified_name(qualifiers, ty.name());
        let Some(target) = self.cache.lookup_type(ty.ty()) else {
            return Some(VariableValue::new(name));
        };

        self.format(value, target, Qualifiers::empty())
            .map(|value| value.ty(name))
    }

    fn format_structure(
        &mut self,
        value: AddrOrValue<'_>,
        ty: StructureType<'_>,
        qualifiers: Qualifiers,
    ) -> Option<VariableValue> {
        let name = ty.name().unwrap_or("<anonymous>");
        let mut result = VariableValue::new(qualified_name(qualifiers, name));
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
                    self.format(value.offset(offset), ty, Qualifiers::empty())
                })
                .unwrap_or_default();

            result = result.member(member.name().unwrap_or("<anonymous>"), ty);
        }

        Some(result)
    }

    /// Formats the NUL-terminated string at `addr`, if `pointee_type` is a single-byte character
    /// type.
    ///
    /// Returns `None` if `pointee_type` is not a single-byte character type, or if the memory at
    /// `addr` is not a valid UTF-8 string.
    fn format_c_string(&self, addr: u64, pointee_type: &Type<'_>) -> Option<JsonValue> {
        /// The maximum number of bytes read for a NUL-terminated string.
        ///
        /// This value was chosen arbitrarily. A string without a NUL terminator in the bytes read
        /// is reported as truncated.
        const MAX_BYTES: usize = 512;

        let Type::Primitive(pointee_type) = pointee_type else {
            return None;
        };

        let pointee_is_char = matches!(pointee_type.size(), Size::Bytes(1) | Size::Bits(8))
            && matches!(
                pointee_type.encoding(),
                Some(PrimitiveTypeEncoding::SignedChar | PrimitiveTypeEncoding::UnsignedChar)
            );
        if !pointee_is_char {
            return None;
        }

        let bytes = self.memory.get_memory_up_to(addr, MAX_BYTES)?;
        CStringValue::parse(bytes).map(JsonValue::from)
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
        Type::Void => "void",
        _ => "<not implemented>",
    }
}

/// Prefixes the name of a non-pointer type with its qualifiers, for example `const char`.
fn qualified_name(qualifiers: Qualifiers, name: &str) -> String {
    if qualifiers.is_empty() {
        name.to_owned()
    } else {
        format!("{qualifiers} {name}")
    }
}

/// Builds the name of a pointer type from the qualified name of its final pointee and the
/// qualifiers of each pointer level, starting with the outermost pointer.
///
/// For example, `const char` with `[empty, CONST]` results in `const char* const*`.
fn pointer_name(base_name: &str, pointer_qualifiers: &[Qualifiers]) -> String {
    let levels = pointer_qualifiers.iter().rev().map(|qualifiers| {
        if qualifiers.is_empty() {
            "*".to_owned()
        } else {
            format!("* {qualifiers}")
        }
    });

    iter::once(base_name.to_owned()).chain(levels).collect()
}

/// A UTF-8 string read from memory up to its NUL terminator.
struct CStringValue<'a> {
    inner: &'a str,
    /// Whether the bytes read contain no NUL terminator.
    truncated: bool,
}

impl<'a> CStringValue<'a> {
    /// Parses a string from `bytes`, which start at the string's address.
    ///
    /// Returns `None` if the bytes before the NUL terminator are not entirely valid UTF-8. If
    /// `bytes` contains no NUL terminator, we attempt to parse the bytes as UTF-8, ignoring any
    /// truncated multibyte character at the end, and indicate the string as truncated.
    fn parse(bytes: &'a [u8]) -> Option<Self> {
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

    fn parse(bytes: &[u8]) -> Option<(&str, bool)> {
        CStringValue::parse(bytes).map(|s| (s.inner, s.truncated))
    }

    #[test]
    fn test_pointer_name_qualifiers() {
        // `const char* const* volatile`: the innermost pointer level comes last in the slice.
        let name = pointer_name(
            &qualified_name(Qualifiers::CONST, "char"),
            &[Qualifiers::VOLATILE, Qualifiers::CONST],
        );
        assert_eq!(name, "const char* const* volatile");
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
