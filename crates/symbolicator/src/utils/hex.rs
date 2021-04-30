use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;

use serde::de::{self, Deserialize, Deserializer};
use serde::ser::{Serialize, Serializer};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct HexValue(pub u64);

impl fmt::Display for HexValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl Serialize for HexValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl FromStr for HexValue {
    type Err = ParseIntError;

    fn from_str(s: &str) -> Result<HexValue, ParseIntError> {
        if s.starts_with("0x") || s.starts_with("0X") {
            u64::from_str_radix(&s[2..], 16).map(HexValue)
        } else {
            s.parse().map(HexValue)
        }
    }
}

impl<'de> Deserialize<'de> for HexValue {
    fn deserialize<D>(deserializer: D) -> Result<HexValue, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HexVisitor;

        impl<'de> de::Visitor<'de> for HexVisitor {
            type Value = HexValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a number or hex string")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(HexValue(v as u64))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(HexValue(v as u64))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse().map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_any(HexVisitor)
    }
}
