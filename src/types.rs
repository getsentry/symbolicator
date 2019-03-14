use std::{collections::BTreeMap, fmt, sync::Arc};

use actix::Message;

use failure::{Backtrace, Fail};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use symbolic::common::{Arch, CodeId, DebugId};

use url::Url;

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    Http {
        id: String,
        #[serde(with = "url_serde")]
        url: Url,

        layout: DirectoryLayout,

        #[serde(default = "FileType::all_vec")]
        filetypes: Vec<FileType>,

        #[serde(default)]
        is_public: bool,
    },
}

impl SourceConfig {
    pub fn is_public(&self) -> bool {
        match *self {
            SourceConfig::Http { is_public, .. } => is_public,
        }
    }

    pub fn id(&self) -> &str {
        match *self {
            SourceConfig::Http { ref id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(untagged)]
pub enum Scope {
    #[serde(rename = "global")]
    Global,
    Scoped(String),
}

impl AsRef<str> for Scope {
    fn as_ref(&self) -> &str {
        match *self {
            Scope::Scoped(ref s) => &s,
            Scope::Global => "global",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Frame {
    pub addr: HexValue,

    pub module: Option<String>, // NOTE: This is "package" in Sentry
    pub name: Option<String>,   // Only present if ?demangle was set
    pub language: Option<String>,
    pub symbol: Option<String>,
    pub symbol_address: Option<HexValue>,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub line_address: Option<HexValue>, // NOTE: This does not exist in Sentry
}

#[derive(Deserialize)]
pub struct ObjectInfo {
    #[serde(rename = "type")]
    pub ty: ObjectType,

    pub debug_id: String,
    pub code_id: Option<String>,

    #[serde(default)]
    pub debug_name: Option<String>,

    #[serde(default)]
    pub code_name: Option<String>,

    pub address: HexValue,

    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ObjectType(String);

#[derive(Clone, Debug, Copy)]
pub struct HexValue(pub u64);

impl<'de> Deserialize<'de> for HexValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let string: &str = Deserialize::deserialize(deserializer)?;
        if string.starts_with("0x") || string.starts_with("0X") {
            if let Ok(x) = u64::from_str_radix(&string[2..], 16) {
                return Ok(HexValue(x));
            }
        }

        Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(string),
            &"a hex string starting with 0x",
        ))
    }
}

impl<'d> fmt::Display for HexValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl Serialize for HexValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

#[derive(Deserialize)]
pub struct SymbolicateFramesRequest {
    pub meta: Meta,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub modules: Vec<ObjectInfo>,
}

#[derive(Clone, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub signal: Option<u32>,
    #[serde(default)]
    pub arch: Arch,
    pub scope: Scope,
}

#[derive(Deserialize)]
pub struct Thread {
    pub registers: BTreeMap<String, HexValue>,
    #[serde(flatten)]
    pub stacktrace: Stacktrace,
}

#[derive(Serialize, Deserialize)]
pub struct Stacktrace {
    pub frames: Vec<Frame>,
}

#[derive(Serialize, Deserialize)]
pub struct ErrorResponse(pub String);

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SymbolicateFramesResponse {
    //Pending {
    //retry_after: usize,
    //},
    Completed {
        stacktraces: Vec<Stacktrace>,
        errors: Vec<ErrorResponse>,
    },
}

impl Message for SymbolicateFramesRequest {
    type Result = Result<SymbolicateFramesResponse, SymbolicationError>;
}

#[derive(Debug, Fail)]
pub enum SymbolicationErrorKind {
    #[fail(display = "failed sending message to symcache actor")]
    Mailbox,

    #[fail(display = "failed to get symcache")]
    SymCache,

    #[fail(display = "failed to parse symcache during symbolication")]
    Parse,

    #[fail(display = "symbol not found")]
    NotFound,

    #[fail(display = "failed to look into cache")]
    Caching,
}

symbolic::common::derive_failure!(
    SymbolicationError,
    SymbolicationErrorKind,
    doc = "Errors during symbolication"
);

/// This type only exists to have a working impl of `Fail` for `Arc<T> where T: Fail`. We cannot
/// contribute a blanket impl upstream because it would conflict with at least this blanket impl
/// from failure: `impl<E: StdError + Send + Sync + 'static> Fail for E`
#[derive(Debug, Clone)]
pub struct ArcFail<T>(pub Arc<T>);

impl<T: Fail> Fail for ArcFail<T> {
    fn cause(&self) -> Option<&Fail> {
        self.0.cause()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.0.backtrace()
    }
}

impl<T: fmt::Display> fmt::Display for ArcFail<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    /// Windows/PDB code files
    PE,
    /// Windows/PDB debug files
    PDB,
    /// Macos/Mach debug files
    MachDebug,
    /// Macos/Mach code files
    MachCode,
    /// Linux/ELF debug files
    ELFDebug,
    /// Linux/ELF code files
    ELFCode,
    /// Breakpad files (this is the reason we have a flat enum for what at first sight could've
    /// been two enums)
    Breakpad,
}

impl FileType {
    #[inline]
    pub fn all() -> &'static [Self] {
        use FileType::*;
        &[PDB, MachDebug, ELFDebug, PE, MachCode, ELFCode, Breakpad]
    }

    #[inline]
    pub fn debug_types() -> &'static [Self] {
        use FileType::*;
        &[PDB, MachDebug, ELFDebug]
    }

    #[inline]
    pub fn code_types() -> &'static [Self] {
        use FileType::*;
        &[PE, MachCode, ELFCode]
    }

    /// Given an object type, returns filetypes in the order they should be tried.
    #[inline]
    pub fn from_object_type(ty: &ObjectType) -> &'static [Self] {
        use FileType::*;
        match &ty.0[..] {
            "macho" | "apple" => &[MachDebug, MachCode, Breakpad],
            "pe" => &[PDB, PE, Breakpad],
            "elf" => &[PDB, PE, Breakpad],
            _ => Self::all(),
        }
    }

    #[inline]
    pub fn all_vec() -> Vec<Self> {
        Self::all().to_vec()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectoryLayout {
    Native,
    Symstore,
}

impl AsRef<str> for FileType {
    fn as_ref(&self) -> &str {
        use FileType::*;
        match *self {
            PE => "pe",
            PDB => "pdb",
            MachDebug => "mach-debug",
            MachCode => "mach-code",
            ELFDebug => "elf-debug",
            ELFCode => "elf-code",
            Breakpad => "breakpad",
        }
    }
}

/// Information to find a Object in external sources and also internal cache.
#[derive(Debug, Clone)]
pub struct ObjectId {
    pub debug_id: Option<DebugId>,
    pub code_id: Option<CodeId>,
    pub debug_name: Option<String>,
    pub code_name: Option<String>,
}

impl ObjectId {
    pub fn get_cache_key(&self) -> String {
        let mut rv = String::new();
        if let Some(ref debug_id) = self.debug_id {
            rv.push_str(&debug_id.to_string());
        }
        rv.push_str("_");
        if let Some(ref code_id) = self.code_id {
            rv.push_str(code_id.as_str());
        }

        rv
    }
}
