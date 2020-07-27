/// Helper macro to implement serialization into base 16 (hex) string
/// representation. Implements `Display` and `Serialize`.
macro_rules! impl_hex_ser {
    ($type:path) => {
        impl ::std::fmt::Display for $type {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{:#x}", self.0)
            }
        }

        impl ::serde::ser::Serialize for $type {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: ::serde::ser::Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }
    };
}

/// Helper macro to implement deserialization from both numeric values or their
/// base16 (hex) / base10 representations as string. Implements `FromStr` and
/// `Deserialize`.
macro_rules! impl_hex_de {
    ($type:path, $num:ident) => {
        impl ::std::str::FromStr for $type {
            type Err = ::std::num::ParseIntError;

            fn from_str(s: &str) -> Result<$type, ::std::num::ParseIntError> {
                if s.starts_with("0x") || s.starts_with("0X") {
                    $num::from_str_radix(&s[2..], 16).map($type)
                } else {
                    $num::from_str_radix(&s, 10).map($type)
                }
            }
        }

        impl<'de> ::serde::de::Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<$type, D::Error>
            where
                D: ::serde::de::Deserializer<'de>,
            {
                struct HexVisitor;

                impl<'de> ::serde::de::Visitor<'de> for HexVisitor {
                    type Value = $type;

                    fn expecting(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        write!(f, "a number or hex string")
                    }

                    fn visit_i64<E: ::serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                        Ok($type(v as $num))
                    }

                    fn visit_u64<E: ::serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                        Ok($type(v as $num))
                    }

                    fn visit_str<E: ::serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        v.parse().map_err(::serde::de::Error::custom)
                    }
                }

                deserializer.deserialize_any(HexVisitor)
            }
        }
    };
}

/// Helper macro to implement serialization from both numeric values or their
/// hex representation as string.
///
/// This implements `Serialize`, `Deserialize`, `Display` and `FromStr`.
/// Serialization will always use a `"0xbeef"` representation of the value.
/// Deserialization supports raw numbers as well as string representations
/// in hex and base10.
macro_rules! impl_hex_serde {
    ($type:path, $num:ident) => {
        impl_hex_ser!($type);
        impl_hex_de!($type, $num);
    };
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct HexValue(pub u64);

impl_hex_serde!(HexValue, u64);
