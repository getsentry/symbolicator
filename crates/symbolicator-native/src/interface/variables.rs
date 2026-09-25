//! Contains types for representing variable values.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use symbolic::debuginfo::VariableKind as SymbolicVariableKind;

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
    formatted: Option<String>,
}

impl Value {
    /// Create a new empty [`Value`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Provide a formatted value on the [`Value`].
    pub fn formatted(self, formatted: Option<String>) -> Self {
        #[expect(clippy::needless_update, reason = "we will add other representations")]
        Self { formatted, ..self }
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
