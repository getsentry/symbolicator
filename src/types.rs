use std::{collections::BTreeMap, fmt, sync::Arc};

use actix::Message;

use failure::{Backtrace, Fail};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use symbolic::common::{Arch, CodeId, DebugId, Language};

use url::Url;

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    Sentry(SentrySourceConfig),
    Http(HttpSourceConfig),
}

#[derive(Deserialize, Clone, Debug)]
pub struct SentrySourceConfig {
    pub id: String,
    #[serde(with = "url_serde")]
    pub url: Url,

    pub token: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct HttpSourceConfig {
    pub id: String,
    #[serde(with = "url_serde")]
    pub url: Url,

    pub layout: DirectoryLayout,

    #[serde(default = "FileType::all_vec")]
    pub filetypes: Vec<FileType>,

    #[serde(default)]
    pub is_public: bool,
}

impl SourceConfig {
    pub fn is_public(&self) -> bool {
        match *self {
            SourceConfig::Http(ref x) => x.is_public,
            SourceConfig::Sentry(_) => false,
        }
    }

    pub fn id(&self) -> &str {
        match *self {
            SourceConfig::Http(ref x) => &x.id,
            SourceConfig::Sentry(ref x) => &x.id,
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

#[derive(Debug, Clone, Deserialize)]
pub struct RawFrame {
    pub instruction_addr: HexValue,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameStatus {
    Symbolicated,
    MissingSymbol,
    UnknownImage,
    MissingDebugFile,
    MalformedDebugFile,
}

/// See semaphore's Frame for docs
#[derive(Debug, Clone, Serialize)]
pub struct SymbolicatedFrame {
    pub instruction_addr: HexValue,
    pub package: Option<String>,
    pub lang: Option<Language>,
    pub symbol: Option<String>,
    pub function: Option<String>,
    pub filename: Option<String>,
    pub abs_path: Option<String>,
    pub lineno: Option<u32>,
    pub sym_addr: Option<HexValue>,

    pub original_index: Option<usize>,

    pub status: FrameStatus,
}

/// See semaphore's DebugImage for docs
#[derive(Deserialize, Debug, Clone)]
pub struct ObjectInfo {
    #[serde(rename = "type")]
    pub ty: ObjectType,

    #[serde(default)]
    pub arch: Arch,

    pub debug_id: String,
    pub code_id: Option<String>,

    #[serde(default)]
    pub debug_file: Option<String>,

    #[serde(default)]
    pub code_file: Option<String>,

    pub image_addr: HexValue,

    #[serde(default)]
    pub image_size: Option<u64>,
}

#[derive(Deserialize, Clone, Debug, Eq, PartialEq)]
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
pub struct SymbolicationRequest {
    pub meta: Meta,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub threads: Vec<RawStacktrace>,
    #[serde(default)]
    pub modules: Vec<ObjectInfo>,
    #[serde(default)]
    pub request: Option<RequestMeta>,
}

#[derive(Deserialize)]
pub struct RequestMeta {
    pub timeout: u64,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub signal: Option<u32>,
    pub scope: Scope,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawStacktrace {
    pub registers: BTreeMap<String, HexValue>,
    pub frames: Vec<RawFrame>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolicatedStacktrace {
    pub frames: Vec<SymbolicatedFrame>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SymbolicationResponse {
    Pending {
        request_id: String,
        retry_after: usize,
    },
    Completed {
        stacktraces: Vec<SymbolicatedStacktrace>,
        modules: Vec<FetchedDebugFile>,
    },
    UnknownRequest {},
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugFileStatus {
    Found,
    Unused,
    MissingDebugFile,
    MalformedDebugFile,
    FetchingFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchedDebugFile {
    pub status: DebugFileStatus,
}

impl Message for SymbolicationRequest {
    type Result = Result<SymbolicationResponse, SymbolicationError>;
}

#[derive(Debug, Fail)]
pub enum SymbolicationErrorKind {
    #[fail(display = "failed sending message to symcache actor")]
    Mailbox,

    #[fail(display = "failed to get symcache")]
    SymCache,

    #[fail(display = "failed to parse symcache during symbolication")]
    Parse,

    #[fail(display = "no debug file found for address")]
    SymCacheNotFound,

    #[fail(display = "no symbol found in debug file")]
    NotFound,

    #[fail(display = "failed to look into cache")]
    Caching,

    #[fail(display = "symbolication took too long")]
    Timeout,
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
            "macho" => &[MachDebug, MachCode, Breakpad],
            "pe" => &[PDB, PE, Breakpad],
            "elf" => &[ELFDebug, ELFCode, Breakpad],
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
