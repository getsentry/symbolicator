use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use symbolic::common::{Arch, CodeId, DebugId, Language};
use symbolicator_service::objects::{AllObjectCandidates, ObjectFeatures};
use symbolicator_service::types::{
    ObjectFileStatus, Platform, RawObjectInfo, Scope, ScrapingConfig,
};
use symbolicator_service::utils::hex::HexValue;
use symbolicator_sources::SourceConfig;
use thiserror::Error;

pub use crate::metrics::StacktraceOrigin;

#[derive(Debug, Clone)]
/// A request for symbolication of multiple stack traces.
pub struct SymbolicateStacktraces {
    /// The event's platform.
    ///
    /// `Platform` is actually too lenient of a type here—a legitimate
    /// native event should have a [`symbolicator_service::types::NativePlatform`].
    /// However, we use the general `Platform` type for now to be resilient against
    /// wrong values making it through.
    pub platform: Option<Platform>,
    /// The scope of this request which determines access to cached files.
    pub scope: Scope,

    /// The signal thrown on certain operating systems.
    ///
    ///  Signal handlers sometimes mess with the runtime stack. This is used to determine whether
    /// the top frame should be fixed or not.
    pub signal: Option<Signal>,

    /// A list of external sources to load debug files.
    pub sources: Arc<[SourceConfig]>,

    /// Where the stacktraces originated from.
    pub origin: StacktraceOrigin,

    /// A list of threads containing stack traces.
    pub stacktraces: Vec<RawStacktrace>,

    /// A list of images that were loaded into the process.
    ///
    /// This list must cover the instruction addresses of the frames in
    /// [`stacktraces`](Self::stacktraces). If a frame is not covered by any image, the frame cannot
    /// be symbolicated as it is not clear which debug file to load.
    pub modules: Vec<CompleteObjectInfo>,

    /// Whether to apply source context for the stack frames.
    pub apply_source_context: bool,

    /// Scraping configuration controling authenticated requests.
    pub scraping: ScrapingConfig,
}

/// The symbolicated crash data.
///
/// It contains the symbolicated stack frames, module information as well as other
/// meta-information about the crash.
///
/// It is publicly documented at <https://getsentry.github.io/symbolicator/api/response/>.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct CompletedSymbolicationResponse {
    /// When the crash occurred.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "chrono::serde::ts_seconds_option"
    )]
    pub timestamp: Option<DateTime<Utc>>,

    /// The signal that caused this crash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<Signal>,

    /// Information about the operating system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_info: Option<SystemInfo>,

    /// True if the process crashed, false if the dump was produced outside of an exception
    /// handler. Only set for minidumps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crashed: Option<bool>,

    /// If the process crashed, the type of crash.  OS- and possibly CPU- specific.  For
    /// example, "EXCEPTION_ACCESS_VIOLATION" (Windows), "EXC_BAD_ACCESS /
    /// KERN_INVALID_ADDRESS" (Mac OS X), "SIGSEGV" (other Unix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash_reason: Option<String>,

    /// A detailed explanation of the crash, potentially in human readable form. This may
    /// include a string representation of the crash reason or application-specific info.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash_details: Option<String>,

    /// If there was an assertion that was hit, a textual representation of that assertion,
    /// possibly including the file and line at which it occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion: Option<String>,

    /// The threads containing symbolicated stack frames.
    pub stacktraces: Vec<CompleteStacktrace>,

    /// A list of images, extended with status information.
    pub modules: Vec<CompleteObjectInfo>,
}

/// OS-specific crash signal value.
// TODO(markus): Also accept POSIX signal name as defined in signal.h
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
pub struct Signal(pub u32);

/// Information on the symbolication status of this frame.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum FrameStatus {
    /// The frame was symbolicated successfully.
    #[default]
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

    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_name: Option<String>,

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

/// A map of register values.
pub type Registers = BTreeMap<String, HexValue>;

fn is_default_value<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// An unsymbolicated frame from a symbolication request.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RawFrame {
    /// The frame's platform.
    ///
    /// `Platform` is actually too lenient of a type here—a legitimate
    /// native frame should have a [`symbolicator_service::types::NativePlatform`].
    /// However, we use the general `Platform` type for now to be resilient against
    /// wrong values making it through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
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

    /// Whether this stack frame's instruction address needs to be adjusted for symbolication.
    ///
    /// Briefly,
    /// * `Some(true)` means that the address will definitely be adjusted;
    /// * `Some(false)` means that the address will definitely not be adjusted;
    /// * `None` means the address may or may not be adjusted based on heuristics and the value
    ///   of this field in other frames in the same stacktrace.
    ///
    /// Internally this is converted to a value of type `AdjustInstructionAddr`. See also the
    /// documentation of `for_frame`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adjust_instruction_addr: Option<bool>,

    /// The index of the frame's function in the Portable PDB method table.
    ///
    /// This is used for dotnet symbolication.
    ///
    /// NOTE: While the concept of a "function index" also exists in WASM,
    /// we don't need it for symbolication. The instruction address is enough
    /// to get the information we need from a WASM debug file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_id: Option<HexValue>,

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

    /// URL to fetch the source code from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_link: Option<String>,

    /// Whether the frame is related to app-code (rather than libraries/dependencies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_app: Option<bool>,

    /// Information about how the raw frame was created.
    #[serde(default, skip_serializing_if = "is_default_value")]
    pub trust: FrameTrust,
}

/// How trustworth the instruction pointer of the frame is.
///
/// During stack walking it is not always possible to exactly be sure of the instruction
/// pointer and thus detected frame, especially if there was not enough Call Frame
/// Information available.  Frames that were detected by scanning may contain dubious
/// information.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum FrameTrust {
    /// Unknown.
    #[default]
    None,
    /// Found by scanning the stack.
    Scan,
    /// Found by scanning the stack using Call Frame Info.
    CfiScan,
    /// Derived from the Frame Pointer.
    Fp,
    /// Derived from the Call Frame Info rules.
    Cfi,
    /// Explicitly provided by an external stack walker (probably on crashing device).
    PreWalked,
    /// Provided by the CPU context (i.e. the registers).
    ///
    /// This is only possible for the topmost, i.e. the crashing, frame as for the other
    /// frames the registers need to be reconstructed when unwinding the stack.
    Context,
}

impl From<minidump_unwind::FrameTrust> for FrameTrust {
    fn from(source: minidump_unwind::FrameTrust) -> Self {
        match source {
            minidump_unwind::FrameTrust::None => FrameTrust::None,
            minidump_unwind::FrameTrust::Scan => FrameTrust::Scan,
            minidump_unwind::FrameTrust::CfiScan => FrameTrust::CfiScan,
            minidump_unwind::FrameTrust::FramePointer => FrameTrust::Fp,
            minidump_unwind::FrameTrust::CallFrameInfo => FrameTrust::Cfi,
            minidump_unwind::FrameTrust::PreWalked => FrameTrust::PreWalked,
            minidump_unwind::FrameTrust::Context => FrameTrust::Context,
        }
    }
}

/// A stack trace containing unsymbolicated stack frames.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct RawStacktrace {
    /// The OS-dependent identifier of the thread.
    #[serde(default)]
    pub thread_id: Option<u64>,

    /// The name of the thread.
    #[serde(default)]
    pub thread_name: Option<String>,

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

/// Whether a frame's instruction address needs to be "adjusted" by subtracting a word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustInstructionAddr {
    /// The frame's address definitely needs to be adjusted.
    Yes,
    /// The frame's address definitely does not need to be adjusted.
    No,
    /// The frame's address might need to be adjusted.
    ///
    /// This defers to the heuristic in
    /// [InstructionInfo::caller_address](symbolic::common::InstructionInfo::caller_address).
    Auto,
}

impl AdjustInstructionAddr {
    /// Returns the adjustment strategy for the given frame.
    ///
    /// If the frame has the
    /// [`adjust_instruction_addr`](RawFrame::adjust_instruction_addr)
    /// field set, this will be [`Yes`](Self::Yes) or [`No`](Self::No) accordingly, otherwise
    /// the given default is used.
    pub fn for_frame(frame: &RawFrame, default: Self) -> Self {
        match frame.adjust_instruction_addr {
            Some(true) => Self::Yes,
            Some(false) => Self::No,
            None => default,
        }
    }

    /// Returns the default adjustment strategy for the given thread.
    ///
    /// This will be [`Yes`](Self::Yes) if any frame in the thread has the
    /// [`adjust_instruction_addr`](RawFrame::adjust_instruction_addr)
    /// field set, otherwise it will be [`Auto`](Self::Auto).
    pub fn default_for_thread(thread: &RawStacktrace) -> Self {
        if thread
            .frames
            .iter()
            .any(|frame| frame.adjust_instruction_addr.is_some())
        {
            AdjustInstructionAddr::Yes
        } else {
            AdjustInstructionAddr::Auto
        }
    }
}

/// Defines the addressing mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub enum AddrMode {
    /// Declares addresses to be absolute with a shared memory space.
    #[default]
    Abs,
    /// Declares an address to be relative to an indexed module.
    Rel(usize),
}

impl fmt::Display for AddrMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AddrMode::Abs => write!(f, "abs"),
            AddrMode::Rel(idx) => write!(f, "rel:{idx}"),
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
            "rel" => Ok(AddrMode::Rel(index)),
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
        AddrMode::from_str(&s).map_err(de::Error::custom)
    }
}
