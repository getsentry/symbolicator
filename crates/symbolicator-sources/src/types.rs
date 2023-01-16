use std::borrow::Cow;
use std::convert::Infallible;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize};
use symbolic::common::{split_path, CodeId, DebugId};

/// A Wrapper around [`glob::Pattern`] that allows de/serialization.
#[derive(Debug, Clone)]
pub struct Glob(pub glob::Pattern);

impl<'de> Deserialize<'de> for Glob {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = Cow::<str>::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom).map(Glob)
    }
}

impl Serialize for Glob {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl Deref for Glob {
    type Target = glob::Pattern;

    fn deref(&self) -> &glob::Pattern {
        &self.0
    }
}

/// The type of an executable object file.
#[derive(Serialize, Clone, Copy, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ObjectType {
    /// ELF Object.
    Elf,
    /// Mach-O Object.
    Macho,
    /// Portable Executable.
    Pe,
    /// A WASM executable.
    Wasm,
    /// Portable Executable containing .NET code, which has a Portable PDB companion.
    PeDotnet,
    /// Unknown Object.
    #[default]
    Unknown,
}

impl FromStr for ObjectType {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<ObjectType, Infallible> {
        Ok(match s {
            "elf" => ObjectType::Elf,
            "macho" => ObjectType::Macho,
            "pe" => ObjectType::Pe,
            "pe_dotnet" => ObjectType::PeDotnet,
            "wasm" => ObjectType::Wasm,
            _ => ObjectType::Unknown,
        })
    }
}

impl<'de> Deserialize<'de> for ObjectType {
    fn deserialize<D>(deserializer: D) -> Result<ObjectType, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: Cow<'de, str> = Deserialize::deserialize(deserializer)?;
        Ok(s.parse().unwrap())
    }
}

impl fmt::Display for ObjectType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ObjectType::Elf => write!(f, "elf"),
            ObjectType::Macho => write!(f, "macho"),
            ObjectType::Pe => write!(f, "pe"),
            ObjectType::PeDotnet => write!(f, "pe_dotnet"),
            ObjectType::Wasm => write!(f, "wasm"),
            ObjectType::Unknown => write!(f, "unknown"),
        }
    }
}

/// Information to find an object in external sources and also internal cache.
#[derive(Debug, Clone, Default)]
pub struct ObjectId {
    /// Identifier of the code file.
    pub code_id: Option<CodeId>,

    /// Path to the code file (executable or library).
    pub code_file: Option<String>,

    /// Identifier of the debug file.
    pub debug_id: Option<DebugId>,

    /// Path to the debug file.
    pub debug_file: Option<String>,

    /// Hint to what we believe the file type should be.
    pub object_type: ObjectType,
}

impl From<DebugId> for ObjectId {
    fn from(source: DebugId) -> Self {
        Self {
            debug_id: Some(source),
            ..Default::default()
        }
    }
}

impl ObjectId {
    /// Basename of the `code_file` field.
    pub fn code_file_basename(&self) -> Option<&str> {
        Some(split_path(self.code_file.as_ref()?).1)
    }

    /// Basename of the `debug_file` field.
    pub fn debug_file_basename(&self) -> Option<&str> {
        Some(split_path(self.debug_file.as_ref()?).1)
    }
}
