//! Contains types for representing variable values.

use std::{collections::HashMap, fmt::Display, num::ParseIntError, str::FromStr};

use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use symbolic::debuginfo::VariableKind as SymbolicVariableKind;
use thiserror::Error;

/// Variables keyed by their names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Variables(HashMap<String, Variable>);

impl Variables {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<S> FromIterator<(S, Variable)> for Variables
where
    S: Into<String>,
{
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (S, Variable)>,
    {
        Self(iter.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

/// A variable extracted from a memory dump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variable {
    kind: VariableKind,
    #[serde(flatten)]
    value: TypedValue,
}

/// The variable's kind (e.g. local or parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableKind {
    Local,
    Parameter,
}

/// A [`Value`] with a type.
///
/// Constructed via [`Value::ty`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TypedValue {
    #[serde(rename = "type")]
    ty: String,
    #[serde(flatten)]
    value: Value,
}

impl TypedValue {
    /// Convert this [`TypedValue`] into a [`Variable`] with the given kind.
    pub fn kind<K>(self, kind: K) -> Variable
    where
        K: Into<VariableKind>,
    {
        let kind = kind.into();
        Variable { kind, value: self }
    }
}

/// A value of a variable or field within a variable (e.g. list item, pointee, struct field).
///
/// This struct allows for multiple co-existing representations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Value {
    /// A fully-formatted, source-specific representation of a variable value.
    ///
    /// This should typically be used for primitive types like integers, booleans, chars.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<JsonValue>,

    /// A pointer address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pointer_address: Option<PointerAddress>,
}

impl Value {
    /// Create a new empty [`Value`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Provide a fully-formatted, source-specific representation of the [`Value`].
    ///
    /// This should typically be used for primitive types like integers, booleans, and chars.
    pub fn value<V>(self, value: V) -> Self
    where
        V: Into<JsonValue>,
    {
        let value = Some(value.into());
        Self { value, ..self }
    }

    pub fn pointer_address<V>(mut self, address: V) -> Self
    where
        V: Into<PointerAddress>,
    {
        self.pointer_address = Some(address.into());
        self
    }

    /// Convert this [`Value`] to a [`TypedValue`] with the given type.
    pub fn ty<T>(self, ty: T) -> TypedValue
    where
        T: Into<String>,
    {
        let ty = ty.into();
        TypedValue { ty, value: self }
    }
}

impl From<SymbolicVariableKind> for VariableKind {
    fn from(value: SymbolicVariableKind) -> Self {
        match value {
            SymbolicVariableKind::Parameter => VariableKind::Parameter,
            SymbolicVariableKind::Local => VariableKind::Local,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum PointerAddress {
    ThirtyTwoBit(u32),
    SixtyFourBit(u64),
}

impl From<u32> for PointerAddress {
    fn from(value: u32) -> Self {
        Self::ThirtyTwoBit(value)
    }
}

impl From<u64> for PointerAddress {
    fn from(value: u64) -> Self {
        Self::SixtyFourBit(value)
    }
}

#[derive(Debug, Error)]
pub(crate) enum PointerAddressParseError {
    #[error("The address had an unexpected legnth.")]
    UnexpectedLength,
    #[error("The address was missing the 0x prefix.")]
    MissingPrefix,
    #[error("Could not parse an integer: {0}.")]
    ParseInt(#[from] ParseIntError),
}

impl Display for PointerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PointerAddress::ThirtyTwoBit(v) => write!(f, "{v:#010x}"),
            PointerAddress::SixtyFourBit(v) => write!(f, "{v:#018x}"),
        }
    }
}

impl FromStr for PointerAddress {
    type Err = PointerAddressParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let stripped = s
            .strip_prefix("0x")
            .ok_or(PointerAddressParseError::MissingPrefix)?;

        Ok(match stripped.len() {
            8 => Self::ThirtyTwoBit(u32::from_str_radix(stripped, 16)?),
            16 => Self::SixtyFourBit(u64::from_str_radix(stripped, 16)?),
            _ => return Err(PointerAddressParseError::UnexpectedLength),
        })
    }
}

impl Serialize for PointerAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PointerAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}
