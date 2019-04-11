use std::collections::BTreeMap;
use std::fmt::{self, Write};
use std::sync::Arc;

use crate::hex::HexValue;

use failure::{Backtrace, Fail};
use serde::{Deserialize, Deserializer, Serialize};
use symbolic::common::{Arch, CodeId, DebugId, Language};
use url::Url;

fn is_default<T: Default + PartialEq>(x: &T) -> bool {
    x == &Default::default()
}

/// Symbolication request identifier.
#[derive(Debug, Clone, Deserialize, Serialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct RequestId(pub String);

/// OS-specific crash signal value.
// TODO(markus): Also accept POSIX signal name as defined in signal.h
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
pub struct Signal(pub u32);

/// Determines the layout of an external source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirectoryLayout {
    /// Uses conventions of native debuggers.
    Native,
    /// Uses Microsoft symbol server conventions.
    Symstore,
}

/// Configuration for an external source.
#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    /// Sentry debug files endpoint.
    Sentry(Arc<SentrySourceConfig>),
    /// Http server implementing the Microsoft Symbol Server protocol.
    Http(Arc<HttpSourceConfig>),
    /// Amazon S3 bucket containing symbols in a directory hierarchy.
    S3(Arc<S3SourceConfig>),
}

/// Configuration for the Sentry-internal debug files endpoint.
#[derive(Deserialize, Clone, Debug)]
pub struct SentrySourceConfig {
    /// Unique source identifier.
    pub id: String,

    /// Absolute URL of the endpoint.
    #[serde(with = "url_serde")]
    pub url: Url,

    /// Bearer authorization token.
    pub token: String,
}

/// Configuration for symbol server HTTP endpoints.
#[derive(Deserialize, Clone, Debug)]
pub struct HttpSourceConfig {
    /// Unique source identifier.
    pub id: String,

    /// Absolute URL of the symbol server.
    #[serde(with = "url_serde")]
    pub url: Url,

    /// Additional headers to be sent to the symbol server with every request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,

    #[serde(flatten)]
    pub files: ExternalSourceConfigBase,
}

/// Deserializes an S3 region string.
fn deserialize_region<'de, D>(deserializer: D) -> Result<rusoto_core::Region, D::Error>
where
    D: Deserializer<'de>,
{
    // For safety reason we only want to parse the default AWS regions so that
    // non AWS services cannot be accessed.
    use serde::de::Error as _;
    let region = String::deserialize(deserializer)?;
    region
        .parse()
        .map_err(|e| D::Error::custom(format!("region: {}", e)))
}

/// Amazon S3 authorization information.
#[derive(Deserialize, Clone, Debug)]
pub struct S3SourceKey {
    /// The region of the S3 bucket.
    #[serde(deserialize_with = "deserialize_region")]
    pub region: rusoto_core::Region,

    /// S3 authorization key.
    pub access_key: String,

    /// S3 secret key.
    pub secret_key: String,
}

impl PartialEq for S3SourceKey {
    fn eq(&self, other: &S3SourceKey) -> bool {
        self.access_key == other.access_key
            && self.secret_key == other.secret_key
            && self.region == other.region
    }
}

impl Eq for S3SourceKey {}

impl std::hash::Hash for S3SourceKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.access_key.hash(state);
        self.secret_key.hash(state);
        self.region.name().hash(state);
    }
}

/// Configuration for S3 symbol buckets.
#[derive(Deserialize, Clone, Debug)]
pub struct S3SourceConfig {
    /// Unique source identifier.
    pub id: String,

    /// Name of the bucket in the S3 account.
    pub bucket: String,

    /// A path from the root of the bucket where files are located.
    #[serde(default)]
    pub prefix: String,

    /// Authorization information for this bucket. Needs read access.
    #[serde(flatten)]
    pub source_key: Arc<S3SourceKey>,

    #[serde(flatten)]
    pub files: ExternalSourceConfigBase,
}

/// Common parameters for external filesystem-like buckets configured by users.
#[derive(Deserialize, Clone, Debug)]
pub struct ExternalSourceConfigBase {
    /// Directory layout of this symbol server.
    pub layout: DirectoryLayout,

    /// File types that are supported by this server.
    #[serde(default = "FileType::all_vec")]
    pub filetypes: Vec<FileType>,

    /// Overwrite filename casing convention of `self.layout`. This is useful in the case of
    /// DirectoryLayout::Symstore, where servers are supposed to handle requests
    /// case-insensitively, but practically don't (in the case of S3 buckets it's not possible),
    /// making this aspect not well-specified.
    #[serde(default)]
    pub casing: FilenameCasing,

    /// Whether debug files are shared across scopes.
    #[serde(default)]
    pub is_public: bool,
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
pub enum FilenameCasing {
    Default,
    Uppercase,
    Lowercase,
}

impl Default for FilenameCasing {
    fn default() -> Self {
        FilenameCasing::Default
    }
}

impl SourceConfig {
    /// The unique identifier of this source.
    pub fn id(&self) -> &str {
        match *self {
            SourceConfig::Http(ref x) => &x.id,
            SourceConfig::S3(ref x) => &x.id,
            SourceConfig::Sentry(ref x) => &x.id,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match *self {
            SourceConfig::Sentry(..) => "sentry",
            SourceConfig::S3(..) => "sentry",
            SourceConfig::Http(..) => "sentry",
        }
    }

    /// Determines whether debug files from this bucket may be shared.
    pub fn is_public(&self) -> bool {
        match *self {
            SourceConfig::Http(ref x) => x.files.is_public,
            SourceConfig::S3(ref x) => x.files.is_public,
            SourceConfig::Sentry(_) => false,
        }
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
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Scope::Global => f.write_str("global"),
            Scope::Scoped(ref scope) => f.write_str(&scope),
        }
    }
}

/// An unsymbolicated frame from a symbolication request.
#[derive(Debug, Clone, Deserialize)]
pub struct RawFrame {
    /// The absolute instruction address of this frame.
    pub instruction_addr: HexValue,
    #[serde(default)]
    pub package: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawStacktrace {
    #[serde(default)]
    pub registers: BTreeMap<String, HexValue>,
    pub frames: Vec<RawFrame>,
}

/// Specification of an image loaded into the process.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ObjectInfo {
    /// Platform image file type (container format).
    #[serde(rename = "type")]
    pub ty: ObjectType,

    /// Identifier of the code file.
    #[serde(skip_serializing_if = "is_default")]
    pub code_id: Option<String>,

    /// Name of the code file.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub code_file: Option<String>,

    /// Identifier of the debug file.
    #[serde(skip_serializing_if = "is_default")]
    pub debug_id: Option<String>,

    /// Name of the debug file.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub debug_file: Option<String>,

    /// Absolute address at which the image was mounted into virtual memory.
    pub image_addr: HexValue,

    /// Size of the image in virtual memory.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub image_size: Option<u64>,
}

/// The type of an object file.
#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct ObjectType(pub String);

/// Information on the symbolication status of this frame.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameStatus {
    /// The frame was symbolicated successfully.
    Symbolicated,
    /// The symbol (i.e. function) was not found within the debug file.
    MissingSymbol,
    /// No debug image is specified for the address of the frame.
    UnknownImage,
    /// The debug file could not be retrieved from any of the sources.
    MissingDebugFile,
    /// The retrieved debug file could not be processed.
    MalformedDebugFile,
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
    /// The instruction address as given in the request.
    pub instruction_addr: HexValue,

    /// The path to the image this frame is located in.
    #[serde(skip_serializing_if = "is_default")]
    pub package: Option<String>,
    /// The language of the symbol (function) this frame is located in.
    #[serde(skip_serializing_if = "is_default")]
    pub lang: Option<Language>,
    /// The mangled name of the function this frame is located in.
    #[serde(skip_serializing_if = "is_default")]
    pub symbol: Option<String>,
    /// Start address of the function this frame is located in (lower or equal to instruction_addr).
    #[serde(skip_serializing_if = "is_default")]
    pub sym_addr: Option<HexValue>,
    /// The demangled function name.
    #[serde(skip_serializing_if = "is_default")]
    pub function: Option<String>,
    /// Source file path relative to the compilation directory.
    #[serde(skip_serializing_if = "is_default")]
    pub filename: Option<String>,
    /// Absolute source file path.
    #[serde(skip_serializing_if = "is_default")]
    pub abs_path: Option<String>,
    /// The line number within the source file, starting at 1 for the first line.
    #[serde(skip_serializing_if = "is_default")]
    pub lineno: Option<u32>,
}

/// A symbolicated stacktrace.
///
/// Frames in this request may or may not be symbolicated. The status field contains information on
/// the individual success for each frame.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SymbolicatedStacktrace {
    /// ID of thread that had this stacktrace. Returned when a minidump was processed.
    #[serde(skip_serializing_if = "is_default")]
    pub thread_id: Option<u64>,

    /// Whether that thread was the crashing one.
    #[serde(skip_serializing_if = "is_default")]
    pub is_crashed_thread: Option<bool>,

    /// Registers, only relevant when returning a processed minidump.
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default")]
    pub registers: BTreeMap<String, HexValue>,

    /// Frames of this stack trace.
    pub frames: Vec<SymbolicatedFrame>,
}

/// Information on a debug information file.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugFileStatus {
    /// The debug information file was found and successfully processed.
    Found,
    /// The debug image was not referenced in the stack trace and not further handled.
    Unused,
    /// The file could not be found in any of the specified sources.
    MissingDebugFile,
    /// The debug file failed to process.
    MalformedDebugFile,
    /// The debug file could not be downloaded.
    FetchingFailed,
    /// The file exceeds the internal download limit.
    TooLarge,
    /// An internal error while handling this image.
    Other,
}

/// Enhanced information on an in
#[derive(Debug, Clone, Serialize)]
pub struct FetchedDebugFile {
    /// Status for handling this debug file.
    pub status: DebugFileStatus,
    /// Actual architecture of this debug file.
    pub arch: Arch,
    /// More information on the object file.
    #[serde(flatten)]
    pub object_info: ObjectInfo,
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
    Completed(CompletedSymbolicationResponse),
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CompletedSymbolicationResponse {
    /// The signal that caused this crash.
    #[serde(skip_serializing_if = "is_default")]
    pub signal: Option<Signal>,

    /// Information about the operating system.
    #[serde(skip_serializing_if = "is_default")]
    pub system_info: Option<SystemInfo>,

    /// True if the process crashed, false if the dump was produced outside of an exception
    /// handler. Only set for minidumps.
    #[serde(skip_serializing_if = "is_default")]
    pub crashed: Option<bool>,

    /// If the process crashed, the type of crash.  OS- and possibly CPU- specific.  For
    /// example, "EXCEPTION_ACCESS_VIOLATION" (Windows), "EXC_BAD_ACCESS /
    /// KERN_INVALID_ADDRESS" (Mac OS X), "SIGSEGV" (other Unix).
    #[serde(skip_serializing_if = "is_default")]
    pub crash_reason: Option<String>,

    /// If there was an assertion that was hit, a textual representation of that assertion,
    /// possibly including the file and line at which it occurred.
    #[serde(skip_serializing_if = "is_default")]
    pub assertion: Option<String>,

    /// The threads containing symbolicated stack frames.
    pub stacktraces: Vec<SymbolicatedStacktrace>,

    /// A list of images, extended with status information.
    pub modules: Vec<FetchedDebugFile>,
}

/// Information about the operating system.
#[derive(Debug, Clone, Serialize, Eq, PartialEq)]
pub struct SystemInfo {
    /// Name of operating system
    pub os_name: String,

    /// Version of operating system
    pub os_version: String,

    /// Internal build number
    pub os_build: String,

    /// OS architecture
    pub cpu_arch: Arch,
}

/// Errors during symbolication
#[derive(Debug, Fail)]
pub enum SymbolicationError {
    #[fail(display = "failed sending message to symcache actor")]
    Mailbox,

    #[fail(display = "symbolication took too long")]
    Timeout,

    #[fail(display = "server panicked (see system log)")]
    Panic,

    #[fail(display = "failed to process minidump")]
    Minidump,
}

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
#[serde(rename_all = "snake_case")]
pub enum FileType {
    /// Windows/PDB code files
    Pe,
    /// Windows/PDB debug files
    Pdb,
    /// Macos/Mach debug files
    MachDebug,
    /// Macos/Mach code files
    MachCode,
    /// Linux/ELF debug files
    ElfDebug,
    /// Linux/ELF code files
    ElfCode,
    /// Breakpad files (this is the reason we have a flat enum for what at first sight could've
    /// been two enums)
    Breakpad,
}

impl FileType {
    /// Lists all available file types.
    #[inline]
    pub fn all() -> &'static [Self] {
        use FileType::*;
        &[Pdb, MachDebug, ElfDebug, Pe, MachCode, ElfCode, Breakpad]
    }

    /// Given an object type, returns filetypes in the order they should be tried.
    #[inline]
    pub fn from_object_type(ty: &ObjectType) -> &'static [Self] {
        use FileType::*;
        match &ty.0[..] {
            "macho" => &[MachDebug, MachCode, Breakpad],
            "pe" => &[Pdb, Pe, Breakpad],
            "elf" => &[ElfDebug, ElfCode, Breakpad],
            _ => Self::all(),
        }
    }

    #[inline]
    fn all_vec() -> Vec<Self> {
        Self::all().to_vec()
    }
}

impl AsRef<str> for FileType {
    fn as_ref(&self) -> &str {
        match *self {
            FileType::Pe => "pe",
            FileType::Pdb => "pdb",
            FileType::MachDebug => "mach_debug",
            FileType::MachCode => "mach_code",
            FileType::ElfDebug => "elf_debug",
            FileType::ElfCode => "elf_code",
            FileType::Breakpad => "breakpad",
        }
    }
}

/// Information to find a Object in external sources and also internal cache.
#[derive(Debug, Clone)]
pub struct ObjectId {
    /// Identifier of the code file.
    pub code_id: Option<CodeId>,

    /// Path to the code file (executable or library).
    pub code_file: Option<String>,

    /// Identifier of the debug file.
    pub debug_id: Option<DebugId>,

    /// Path to the debug file.
    pub debug_file: Option<String>,
}

impl ObjectId {
    /// Returns the cache key for this object.
    pub fn cache_key(&self) -> String {
        let mut rv = String::new();
        if let Some(ref debug_id) = self.debug_id {
            rv.push_str(&debug_id.to_string());
        }
        rv.push_str("_");
        if let Some(ref code_id) = self.code_id {
            write!(rv, "{}", code_id).ok();
        }

        rv
    }
}
