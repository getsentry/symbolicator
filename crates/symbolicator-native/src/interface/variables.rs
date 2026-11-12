//! Contains types for representing variable values.

use std::collections::BTreeMap;
use std::fmt::Display;

use serde::Serialize;
use serde_json::Value as JsonValue;
use symbolic::debuginfo::VariableKind as SymbolicVariableKind;

/// Variables keyed by their names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Variables(BTreeMap<String, Variable>);

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Variable {
    kind: VariableKind,
    #[serde(flatten)]
    value: Value,
}

/// The variable's kind (e.g. local or parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableKind {
    Local,
    Parameter,
}

/// A value of a variable or field within a variable (e.g. list item, pointee, struct field).
///
/// This struct allows for multiple co-existing representations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Value {
    /// The type name.
    #[serde(rename = "type")]
    ty: String,

    /// A fully-formatted, source-specific representation of a variable value.
    ///
    /// This should typically be used for primitive types like integers, booleans, chars.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<JsonValue>,

    /// A pointer address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pointer_address: Option<PointerAddress>,

    /// The value after the pointer has been dereferenced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pointee: Option<Box<Value>>,
}

impl Value {
    /// Create a new named [`Value`].
    pub fn new<T>(ty: T) -> Self
    where
        T: Into<String>,
    {
        Self {
            ty: ty.into(),
            ..Default::default()
        }
    }

    /// Provide a fully-formatted, source-specific representation of the [`Value`].
    ///
    /// This should typically be used for primitive types like integers, booleans, and chars.
    pub fn value<V>(self, value: V) -> Self
    where
        V: Into<JsonValue>,
    {
        Self {
            value: Some(value.into()),
            ..self
        }
    }

    /// Sets the pointer address on this [`Value`].
    pub fn pointer_address<V>(mut self, address: V) -> Self
    where
        V: Into<PointerAddress>,
    {
        self.pointer_address = Some(address.into());
        self
    }

    /// Sets the pointee on this [`Value`].
    ///
    /// Can be set to include the dereferenced variable value of a pointer.
    pub fn pointee<T>(mut self, pointee: T) -> Self
    where
        T: Into<Box<Self>>,
    {
        self.pointee = Some(pointee.into());
        self
    }

    /// Convert this [`Value`] into a [`Variable`] with the given kind.
    pub fn into_variable<K>(self, kind: K) -> Variable
    where
        K: Into<VariableKind>,
    {
        Variable {
            kind: kind.into(),
            value: self,
        }
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
pub enum PointerAddress {
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

impl From<PointerAddress> for u64 {
    fn from(value: PointerAddress) -> Self {
        match value {
            PointerAddress::ThirtyTwoBit(v) => v.into(),
            PointerAddress::SixtyFourBit(v) => v,
        }
    }
}

impl Display for PointerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PointerAddress::ThirtyTwoBit(v) => write!(f, "{v:#010x}"),
            PointerAddress::SixtyFourBit(v) => write!(f, "{v:#018x}"),
        }
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
