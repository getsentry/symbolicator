//! Types for the Symbolicator API.
//!
//! This module contains some types which (de)serialise to/from JSON to make up the public
//! HTTP API.  Its messy and things probably need a better place and different way to signal
//! they are part of the public API.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{de, Deserialize, Deserializer, Serialize};
use symbolic::common::{split_path, Arch, CodeId, DebugId, Language};
use symbolic::debuginfo::Object;
use uuid::Uuid;

use crate::utils::addr::AddrMode;
use crate::utils::hex::HexValue;
use crate::utils::sentry::ConfigureScope;

mod objects;

pub use objects::{AllObjectCandidates, ObjectCandidate, ObjectDownloadInfo, ObjectUseInfo};

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
            Scope::Scoped(ref s) => s,
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
            Scope::Scoped(ref scope) => f.write_str(scope),
        }
    }
}

/// Extra JSON request data for multipart requests.
///
/// Multipart requests like `/minidump` and `/applecrashreport` often need some extra
/// request data together with their main data payload which is included as a JSON-formatted
/// multi-part.  This can represent this data.
///
/// This is meant to be extensible, it is conceivable that the existing `sources` mutli-part
/// would merge into this one at some point.
#[derive(Debug, Deserialize)]
pub struct RequestData {
    /// Common symbolication per-request options.
    #[serde(default)]
    pub options: RequestOptions,
}

/// Common options for all symbolication API requests.
///
/// These options control some features which control the symbolication and general request
/// handling behaviour.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RequestOptions {
    /// Whether to return detailed information on DIF object candidates.
    ///
    /// Symbolication requires DIF object files and which ones selected and not selected
    /// influences the quality of symbolication.  Enabling this will return extra
    /// information in the modules list section of the response detailing all DIF objects
    /// considered, any problems with them and what they were used for.  See the
    /// [`ObjectCandidate`] struct for which extra information is returned for DIF objects.
    #[serde(default)]
    pub dif_candidates: bool,

    /// Whether to run the new stackwalking method in addition to the old one and compare their results.
    #[serde(default)]
    pub compare_stackwalking_methods: bool,

    /// Stackwalk with rust-minidump only. Overrides ``compare_stackwalking_methods``.
    #[serde(default)]
    pub rust_minidump: bool,
}

/// A map of register values.
pub type Registers = BTreeMap<String, HexValue>;

fn is_default_value<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// An unsymbolicated frame from a symbolication request.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq)]
pub struct RawFrame {
    /// Controls the addressing mode for [`instruction_addr`](Self::instruction_addr) and
    /// [`sym_addr`](Self::sym_addr).
    ///
    /// If not defined, it defaults to [`AddrMode::Abs`]. The mode can be set to `"rel:INDEX"` to
    /// make the address relative to the module at the given index ([`AddrMode::Rel`]).
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub addr_mode: AddrMode,

    /// The absolute instruction address of this frame.
    ///
    /// See [`addr_mode`](Self::addr_mode) for the exact behavior of addresses.
    pub instruction_addr: HexValue,

    /// The path to the [module](RawObjectInfo) this frame is located in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,

    /// The language of the symbol (function) this frame is located in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<Language>,

    /// The mangled name of the function this frame is located in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,

    /// Start address of the function this frame is located in (lower or equal to
    /// [`instruction_addr`](Self::instruction_addr)).
    ///
    /// See [`addr_mode`](Self::addr_mode) for the exact behavior of addresses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sym_addr: Option<HexValue>,

    /// The demangled function name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,

    /// Source file path relative to the compilation directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,

    /// Absolute path to the source file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abs_path: Option<String>,

    /// The line number within the source file, starting at `1` for the first line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineno: Option<u32>,

    /// Source context before the context line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_context: Vec<String>,

    /// The context line if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_line: Option<String>,

    /// Post context after the context line
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_context: Vec<String>,

    /// Information about how the raw frame was created.
    #[serde(skip_serializing_if = "is_default_value")]
    pub trust: FrameTrust,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FrameTrust {
    None,
    Scan,
    CfiScan,
    Fp,
    Cfi,
    PreWalked,
    Context,
}

impl Default for FrameTrust {
    fn default() -> Self {
        FrameTrust::None
    }
}

impl From<minidump_processor::FrameTrust> for FrameTrust {
    fn from(source: minidump_processor::FrameTrust) -> Self {
        match source {
            minidump_processor::FrameTrust::None => FrameTrust::None,
            minidump_processor::FrameTrust::Scan => FrameTrust::Scan,
            minidump_processor::FrameTrust::CfiScan => FrameTrust::CfiScan,
            minidump_processor::FrameTrust::FramePointer => FrameTrust::Fp,
            minidump_processor::FrameTrust::CallFrameInfo => FrameTrust::Cfi,
            minidump_processor::FrameTrust::PreWalked => FrameTrust::PreWalked,
            minidump_processor::FrameTrust::Context => FrameTrust::Context,
        }
    }
}

/// A stack trace containing unsymbolicated stack frames.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct RawStacktrace {
    /// The OS-dependent identifier of the thread.
    #[serde(default)]
    pub thread_id: Option<u64>,

    /// `true` if this thread triggered the report. Usually indicates that this trace crashed.
    #[serde(default)]
    pub is_requesting: Option<bool>,

    /// Values of CPU registers in the top frame in the trace.
    #[serde(default)]
    pub registers: Registers,

    /// A list of unsymbolicated stack frames.
    ///
    /// The first entry in the list is the active frame, with its callers below.
    pub frames: Vec<RawFrame>,
}

/// Specification of a module loaded into the process.
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
    ///
    /// We do allow the `image_addr` to be skipped if it is zero. This is because systems like WASM
    /// do not require modules to be mounted at a specific absolute address. Per definition, a
    /// module mounted at `0` does not support absolute addressing.
    #[serde(default)]
    pub image_addr: HexValue,

    /// Size of the image in virtual memory.
    ///
    /// The size is infered from the module list if not specified.
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
    Wasm,
    Unknown,
}

impl FromStr for ObjectType {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<ObjectType, Infallible> {
        Ok(match s {
            "elf" => ObjectType::Elf,
            "macho" => ObjectType::Macho,
            "pe" => ObjectType::Pe,
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
            ObjectType::Wasm => write!(f, "wasm"),
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
#[derive(Debug, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
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
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
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

/// Normalized [`RawObjectInfo`] with status attached.
///
/// This describes an object in the modules list of a response to a symbolication request.
///
/// [`RawObjectInfo`] is what the user sends and [`CompleteObjectInfo`] is what the user
/// gets.
#[derive(Debug, Clone, Serialize, Eq, PartialEq, Deserialize)]
pub struct CompleteObjectInfo {
    /// Status for fetching the file with debug info.
    pub debug_status: ObjectFileStatus,

    /// Status for fetching the file with unwind info (for minidump stackwalking).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unwind_status: Option<ObjectFileStatus>,

    /// Features available during symbolication.
    pub features: ObjectFeatures,

    /// Actual architecture of this debug file.
    pub arch: Arch,

    /// More information on the object file.
    #[serde(flatten)]
    pub raw: RawObjectInfo,

    /// More information about the DIF files which were consulted for this object file.
    ///
    /// For stackwalking and symbolication we need various Debug Information Files about
    /// this module.  We look for these DIF files in various locations, this describes all
    /// the DIF files we looked up and what we know about them, how we used them.  It can be
    /// helpful to understand what information was available or missing and for which
    /// reasons.
    ///
    /// This list is not serialised if it is empty.
    #[serde(skip_serializing_if = "AllObjectCandidates::is_empty", default)]
    pub candidates: AllObjectCandidates,
}

impl CompleteObjectInfo {
    /// Given an absolute address converts it into a relative one.
    ///
    /// If it does not fit into the object `None` is returned.
    pub fn abs_to_rel_addr(&self, addr: u64) -> Option<u64> {
        if self.supports_absolute_addresses() {
            addr.checked_sub(self.raw.image_addr.0)
        } else {
            None
        }
    }

    /// Given a relative address returns the absolute address.
    ///
    /// Certain environments do not support absolute addresses in which
    /// case this returns `None`.
    pub fn rel_to_abs_addr(&self, addr: u64) -> Option<u64> {
        if self.supports_absolute_addresses() {
            self.raw.image_addr.0.checked_add(addr)
        } else {
            None
        }
    }

    /// Checks if this image supports absolute addressing.
    ///
    /// Per definition images at 0 do not support absolute addresses.
    pub fn supports_absolute_addresses(&self) -> bool {
        self.raw.image_addr.0 != 0
    }
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
            candidates: AllObjectCandidates::default(),
        }
    }
}

/// The response of a symbolication request or poll request.
#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// The symbolicated crash data.
///
/// It contains the symbolicated stack frames, module information as well as other
/// meta-information about the crash.
///
/// This object is the main type containing the symblicated crash as returned by the
/// `/minidump`, `/symbolicate` and `/applecrashreport` endpoints.  It is publicly
/// documented at <https://getsentry.github.io/symbolicator/api/response/>.  For the actual
/// HTTP response this is further wrapped in [`SymbolicationResponse`] which can also return a
/// pending or failed state etc instead of a result.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct CompletedSymbolicationResponse {
    /// When the crash occurred.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "chrono::serde::ts_seconds_option"
    )]
    pub timestamp: Option<DateTime<Utc>>,

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

impl CompletedSymbolicationResponse {
    /// Clears out all the information about the DIF object candidates in the modules list.
    ///
    /// This will avoid this from being serialised as the DIF object candidates list is not
    /// serialised when it is empty.
    pub fn clear_dif_candidates(&mut self) {
        for module in self.modules.iter_mut() {
            module.candidates.clear()
        }
    }
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

/// Information to find an object in external sources and also internal cache.
///
/// See [`ObjectId::match_object`] for how these can be compared.
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
    pub fn code_file_basename(&self) -> Option<&str> {
        Some(split_path(self.code_file.as_ref()?).1)
    }

    pub fn debug_file_basename(&self) -> Option<&str> {
        Some(split_path(self.debug_file.as_ref()?).1)
    }

    /// Validates that the object matches expected identifiers.
    pub fn match_object(&self, object: &Object<'_>) -> bool {
        if let Some(ref debug_id) = self.debug_id {
            let parsed_id = object.debug_id();

            // Microsoft symbol server sometimes stores updated files with a more recent
            // (=higher) age, but resolves it for requests with lower ages as well. Thus, we
            // need to check whether the parsed debug file fullfills the *miniumum* age bound.
            // For example:
            // `4A236F6A0B3941D1966B41A4FC77738C2` is reported as
            // `4A236F6A0B3941D1966B41A4FC77738C4` from the server.
            //                                  ^
            return parsed_id.uuid() == debug_id.uuid()
                && parsed_id.appendix() >= debug_id.appendix();
        }

        if let Some(ref code_id) = self.code_id {
            if let Some(ref object_code_id) = object.code_id() {
                if object_code_id != code_id {
                    return false;
                }
            }
        }

        true
    }
}

impl ConfigureScope for ObjectId {
    fn to_scope(&self, scope: &mut sentry::Scope) {
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
