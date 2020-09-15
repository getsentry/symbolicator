use std::borrow::Cow;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize};
use symbolic::common::{split_path, Arch, CodeId, DebugId, Language};
use symbolic::minidump::processor::FrameTrust;
use uuid::Uuid;

use crate::utils::hex::HexValue;
use crate::utils::sentry::WriteSentryScope;

/// Symbolication task identifier.
#[derive(Debug, Clone, Copy, Serialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct RequestId(Uuid);

impl RequestId {
    /// Creates a new symbolication task identifier.
    pub fn new(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let uuid = Uuid::deserialize(deserializer);
        Ok(Self(uuid.unwrap_or_default()))
    }
}

/// OS-specific crash signal value.
// TODO(markus): Also accept POSIX signal name as defined in signal.h
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
pub struct Signal(pub u32);

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

impl Deref for Glob {
    type Target = glob::Pattern;

    fn deref(&self) -> &glob::Pattern {
        &self.0
    }
}

/// The scope of a source or debug file.
///
/// Based on scopes, access to debug files that have been cached is determined. If a file comes from
/// a public source, it can be used for any symbolication request. Otherwise, the symbolication
/// request must match the scope of a file.
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
            Scope::Global => "global",
            Scope::Scoped(ref s) => &s,
        }
    }
}

impl Default for Scope {
    fn default() -> Self {
        Scope::Global
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Scope::Global => f.write_str("global"),
            Scope::Scoped(ref scope) => f.write_str(&scope),
        }
    }
}

/// A map of register values.
pub type Registers = BTreeMap<String, HexValue>;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_frame_trust(trust: &FrameTrust) -> bool {
    *trust == FrameTrust::None
}

/// An unsymbolicated frame from a symbolication request.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawFrame {
    /// The absolute instruction address of this frame.
    pub instruction_addr: HexValue,

    /// The path to the image this frame is located in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,

    /// The language of the symbol (function) this frame is located in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<Language>,

    /// The mangled name of the function this frame is located in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,

    /// Start address of the function this frame is located in (lower or equal to instruction_addr).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sym_addr: Option<HexValue>,

    /// The demangled function name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,

    /// Source file path relative to the compilation directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,

    /// Absolute source file path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abs_path: Option<String>,

    /// The line number within the source file, starting at 1 for the first line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineno: Option<u32>,

    /// Source context before the context line
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_context: Vec<String>,

    /// The context line if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_line: Option<String>,

    /// Post context after the context line
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_context: Vec<String>,

    /// Information about how the raw frame was created.
    #[serde(default, skip_serializing_if = "is_default_frame_trust")]
    pub trust: FrameTrust,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RawStacktrace {
    #[serde(default)]
    pub thread_id: Option<u64>,

    #[serde(default)]
    pub is_requesting: Option<bool>,

    #[serde(default)]
    pub registers: Registers,

    pub frames: Vec<RawFrame>,
}

/// Specification of an image loaded into the process.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct RawObjectInfo {
    /// Platform image file type (container format).
    #[serde(rename = "type")]
    pub ty: ObjectType,

    /// Identifier of the code file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_id: Option<String>,

    /// Name of the code file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_file: Option<String>,

    /// Identifier of the debug file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_id: Option<String>,

    /// Name of the debug file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_file: Option<String>,

    /// Absolute address at which the image was mounted into virtual memory.
    pub image_addr: HexValue,

    /// Size of the image in virtual memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_size: Option<u64>,
}

/// The type of an object file.
#[derive(Serialize, Clone, Copy, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    Elf,
    Macho,
    Pe,
    Unknown,
}

impl FromStr for ObjectType {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<ObjectType, Infallible> {
        Ok(match s {
            "elf" => ObjectType::Elf,
            "macho" => ObjectType::Macho,
            "pe" => ObjectType::Pe,
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
            ObjectType::Unknown => write!(f, "unknown"),
        }
    }
}

impl Default for ObjectType {
    fn default() -> ObjectType {
        ObjectType::Unknown
    }
}

/// Information on the symbolication status of this frame.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameStatus {
    /// The frame was symbolicated successfully.
    Symbolicated,
    /// The symbol (i.e. function) was not found within the debug file.
    MissingSymbol,
    /// No debug image is specified for the address of the frame.
    UnknownImage,
    /// The debug file could not be retrieved from any of the sources.
    Missing,
    /// The retrieved debug file could not be processed.
    Malformed,
}

impl Default for FrameStatus {
    fn default() -> Self {
        FrameStatus::Symbolicated
    }
}

/// A potentially symbolicated frame in the symbolication response.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SymbolicatedFrame {
    /// Symbolication status of this frame.
    pub status: FrameStatus,

    /// The index of this frame in the request.
    ///
    /// This is relevant for two reasons:
    ///  1. Frames might disappear if the symbolicator determines them as a false-positive from
    ///     stackwalking without CFI.
    ///  2. Frames might expand to multiple inline frames at the same instruction address. However,
    ///     this might occur within recursion, so the instruction address is not a good
    pub original_index: Option<usize>,

    #[serde(flatten)]
    pub raw: RawFrame,
}

/// A symbolicated stacktrace.
///
/// Frames in this request may or may not be symbolicated. The status field contains information on
/// the individual success for each frame.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CompleteStacktrace {
    /// ID of thread that had this stacktrace. Returned when a minidump was processed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<u64>,

    /// If a dump was produced as a result of a crash, this will point to the thread that crashed.
    /// If the dump was produced by user code without crashing, and the dump contains extended
    /// Breakpad information, this will point to the thread that requested the dump.
    ///
    /// Currently only `Some` for minidumps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_requesting: Option<bool>,

    /// Registers, only useful when returning a processed minidump.
    #[serde(default, skip_serializing_if = "Registers::is_empty")]
    pub registers: Registers,

    /// Frames of this stack trace.
    pub frames: Vec<SymbolicatedFrame>,
}

/// Information on a debug information file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFileStatus {
    /// The file was found and successfully processed.
    Found,
    /// The image was not referenced in the stack trace and not further handled.
    Unused,
    /// The file could not be found in any of the specified sources.
    Missing,
    /// The file failed to process.
    Malformed,
    /// The file could not be downloaded.
    FetchingFailed,
    /// Downloading or processing the file took too long.
    Timeout,
    /// An internal error while handling this image.
    Other,
}

impl ObjectFileStatus {
    pub fn name(self) -> &'static str {
        // used for metrics
        match self {
            ObjectFileStatus::Found => "found",
            ObjectFileStatus::Unused => "unused",
            ObjectFileStatus::Missing => "missing",
            ObjectFileStatus::Malformed => "malformed",
            ObjectFileStatus::FetchingFailed => "fetching_failed",
            ObjectFileStatus::Timeout => "timeout",
            ObjectFileStatus::Other => "other",
        }
    }
}

impl Default for ObjectFileStatus {
    fn default() -> Self {
        ObjectFileStatus::Unused
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObjectFeatures {
    /// The object file contains full debug info.
    pub has_debug_info: bool,

    /// The object file contains unwind info.
    pub has_unwind_info: bool,

    /// The object file contains a symbol table.
    pub has_symbols: bool,

    /// The object file had sources available.
    #[serde(default)]
    pub has_sources: bool,
}

impl ObjectFeatures {
    pub fn merge(&mut self, other: ObjectFeatures) {
        self.has_debug_info |= other.has_debug_info;
        self.has_unwind_info |= other.has_unwind_info;
        self.has_symbols |= other.has_symbols;
        self.has_sources |= other.has_sources;
    }
}

/// Normalized RawObjectInfo with status attached.
///
/// RawObjectInfo is what the user sends and CompleteObjectInfo is what the user gets.
#[derive(Debug, Clone, Serialize, Eq, PartialEq, Deserialize)]
pub struct CompleteObjectInfo {
    /// Status for fetching the file with debug info.
    pub debug_status: ObjectFileStatus,

    /// Status for fetching the file with unwind info (for minidump stackwalking).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unwind_status: Option<ObjectFileStatus>,

    /// Features available during symbolication.
    pub features: ObjectFeatures,

    /// Actual architecture of this debug file.
    pub arch: Arch,

    /// More information on the object file.
    #[serde(flatten)]
    pub raw: RawObjectInfo,
}

impl From<RawObjectInfo> for CompleteObjectInfo {
    fn from(mut raw: RawObjectInfo) -> Self {
        raw.debug_id = raw
            .debug_id
            .filter(|id| !id.is_empty())
            .and_then(|id| id.parse::<DebugId>().ok())
            .map(|id| id.to_string());

        raw.code_id = raw
            .code_id
            .filter(|id| !id.is_empty())
            .and_then(|id| id.parse::<CodeId>().ok())
            .map(|id| id.to_string());

        CompleteObjectInfo {
            debug_status: ObjectFileStatus::Unused,
            unwind_status: None,
            features: ObjectFeatures::default(),
            arch: Arch::Unknown,
            raw,
        }
    }
}

/// The response of a symbolication request or poll request.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SymbolicationResponse {
    /// Symbolication is still running.
    Pending {
        /// The id with which further updates can be polled.
        request_id: RequestId,
        /// An indication when the next poll would be suitable.
        retry_after: usize,
    },
    Completed(Box<CompletedSymbolicationResponse>),
    Failed {
        message: String,
    },
    Timeout,
    InternalError,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CompletedSymbolicationResponse {
    /// When the crash occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,

    /// The signal that caused this crash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<Signal>,

    /// Information about the operating system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_info: Option<SystemInfo>,

    /// True if the process crashed, false if the dump was produced outside of an exception
    /// handler. Only set for minidumps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crashed: Option<bool>,

    /// If the process crashed, the type of crash.  OS- and possibly CPU- specific.  For
    /// example, "EXCEPTION_ACCESS_VIOLATION" (Windows), "EXC_BAD_ACCESS /
    /// KERN_INVALID_ADDRESS" (Mac OS X), "SIGSEGV" (other Unix).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crash_reason: Option<String>,

    /// A detailed explanation of the crash, potentially in human readable form. This may
    /// include a string representation of the crash reason or application-specific info.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crash_details: Option<String>,

    /// If there was an assertion that was hit, a textual representation of that assertion,
    /// possibly including the file and line at which it occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assertion: Option<String>,

    /// The threads containing symbolicated stack frames.
    pub stacktraces: Vec<CompleteStacktrace>,

    /// A list of images, extended with status information.
    pub modules: Vec<CompleteObjectInfo>,
}

/// Information about the operating system.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SystemInfo {
    /// Name of operating system
    pub os_name: String,

    /// Version of operating system
    pub os_version: String,

    /// Internal build number
    pub os_build: String,

    /// OS architecture
    pub cpu_arch: Arch,

    /// Device model name
    pub device_model: String,
}

/// Information to find a Object in external sources and also internal cache.
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

impl ObjectId {
    pub fn code_file_basename(&self) -> Option<&str> {
        Some(split_path(self.code_file.as_ref()?).1)
    }

    pub fn debug_file_basename(&self) -> Option<&str> {
        Some(split_path(self.debug_file.as_ref()?).1)
    }
}

impl WriteSentryScope for ObjectId {
    fn write_sentry_scope(&self, scope: &mut sentry::Scope) {
        scope.set_tag(
            "object_id.code_id",
            self.code_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "None".to_string()),
        );
        scope.set_tag(
            "object_id.code_file_basename",
            self.code_file_basename().unwrap_or("None"),
        );
        scope.set_tag(
            "object_id.debug_id",
            self.debug_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "None".to_string()),
        );
        scope.set_tag(
            "object_id.debug_file_basename",
            self.debug_file_basename().unwrap_or("None"),
        );
        scope.set_tag("object_id.object_type", self.object_type.to_string());
    }
}
