use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserialize, Deserializer};
use serde::ser::{Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum AddrMode {
    Abs,
    ModRel(usize),
}

impl Default for AddrMode {
    fn default() -> AddrMode {
        AddrMode::Abs
    }
}

impl fmt::Display for AddrMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AddrMode::Abs => write!(f, "abs"),
            AddrMode::ModRel(idx) => write!(f, "rel:{}", idx),
        }
    }
}

impl Serialize for AddrMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Debug, Error)]
#[error("invalid address mode")]
pub struct ParseAddrModeError;

impl FromStr for AddrMode {
    type Err = ParseAddrModeError;

    fn from_str(s: &str) -> Result<AddrMode, ParseAddrModeError> {
        if s == "abs" {
            return Ok(AddrMode::Abs);
        }
        let mut iter = s.splitn(2, ':');
        let kind = iter.next().ok_or(ParseAddrModeError)?;
        let index = iter
            .next()
            .and_then(|x| x.parse().ok())
            .ok_or(ParseAddrModeError)?;
        match kind {
            "rel" => Ok(AddrMode::ModRel(index)),
            _ => Err(ParseAddrModeError),
        }
    }
}

impl<'de> Deserialize<'de> for AddrMode {
    fn deserialize<D>(deserializer: D) -> Result<AddrMode, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = Cow::<str>::deserialize(deserializer).map_err(de::Error::custom)?;
        Ok(AddrMode::from_str(&s).map_err(de::Error::custom)?)
    }
}
