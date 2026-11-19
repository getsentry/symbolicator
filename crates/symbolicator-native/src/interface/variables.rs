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

/// A value of a variable or field within a variable (e.g. list item, struct field).
///
/// This struct allows for multiple co-existing representations.
///
/// # Pointers
///
/// A pointer and the values it points to are represented by a single [`Value`]. `type` is the
/// pointer type (e.g. `Player**`), `pointer_address` holds the pointer chain, and `value` or
/// `members` hold the final pointee as they would for a variable of the pointee type.
///
/// `pointer_address` holds one address per pointer level, outermost first. The first address is
/// the one stored in the variable, and each following address is read from the memory at the
/// previous address. The chain ends at the first address that cannot be dereferenced, because
/// it is null (any address below `0x1000`) or because its memory is not in the memory dump. This
/// address is the last entry, and `value` and `members` are not set. As a result, the chain has
/// fewer entries than the pointer levels in `type` when an inner pointer cannot be dereferenced.
///
/// A single-level pointer to a one-byte character primitive (`char`, `signed char`, or
/// `unsigned char`) is shown as a string in `value` when the bytes at its address, up to the
/// NUL terminator, are valid UTF-8. At most 512 bytes are read. If they contain no NUL
/// terminator, the string ends with `... (truncated)`. If the bytes are not valid UTF-8, the
/// pointee is shown as a single character.
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

    /// The addresses of a pointer chain, one per pointer level, outermost first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pointer_address: Vec<PointerAddress>,

    /// Members of a structure, in declaration order.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "vec_tuple_as_object"
    )]
    members: Vec<(String, Value)>,
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

    /// Sets the type name of this [`Value`].
    pub fn ty<T>(mut self, name: T) -> Self
    where
        T: Into<String>,
    {
        self.ty = name.into();
        self
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

    /// Prepends the outer levels of a pointer chain to this [`Value`]'s pointer chain.
    ///
    /// The pointer chain of a pointee that is itself a pointer (e.g. through a typedef) continues
    /// the chain of the pointer that points to it.
    pub fn prepend_pointer_addresses(mut self, mut addresses: Vec<PointerAddress>) -> Self {
        addresses.append(&mut self.pointer_address);
        self.pointer_address = addresses;
        self
    }

    /// Adds a structure member to this [`Value`].
    pub fn member<N>(mut self, name: N, value: Self) -> Self
    where
        N: Into<String>,
    {
        self.members.push((name.into(), value));
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

fn vec_tuple_as_object<S, T>(v: &[(String, T)], s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: Serialize,
{
    s.collect_map(v.iter().map(|(k, v)| (k, v)))
}
