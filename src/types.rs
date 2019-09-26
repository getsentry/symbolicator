use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::{self, Write};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use crate::hex::HexValue;
use crate::sentry::WriteSentryScope;

use failure::{Backtrace, Fail};
use serde::{de, Deserialize, Deserializer, Serialize};
use symbolic::common::{split_path, Arch, CodeId, DebugId, Language};
use symbolic::minidump::processor::FrameTrust;
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
    /// A google cloud storage bucket.
    Gcs(Arc<GcsSourceConfig>),
    /// Local file system.
    Filesystem(Arc<FilesystemSourceConfig>),
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

/// Configuration for reading from the local file system.
#[derive(Deserialize, Clone, Debug)]
pub struct FilesystemSourceConfig {
    /// Unique source identifier.
    pub id: String,

    /// Path to symbol directory.
    pub path: PathBuf,

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

/// GCS authorization information.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct GcsSourceKey {
    /// Gcs authorization key.
    pub private_key: String,

    /// The client email.
    pub client_email: String,
}

/// Configuration for a GCS symbol buckets.
#[derive(Deserialize, Clone, Debug)]
pub struct GcsSourceConfig {
    /// Unique source identifier.
    pub id: String,

    /// Name of the GCS bucket.
    pub bucket: String,

    /// A path from the root of the bucket where files are located.
    #[serde(default)]
    pub prefix: String,

    /// Authorization information for this bucket. Needs read access.
    #[serde(flatten)]
    pub source_key: Arc<GcsSourceKey>,

    #[serde(flatten)]
    pub files: ExternalSourceConfigBase,
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
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct ExternalSourceConfigBase {
    /// Influence whether this source will be selected
    pub filters: SourceFilters,

    /// How files are laid out in this storage.
    pub layout: DirectoryLayout,

    /// Whether debug files are shared across scopes.
    pub is_public: bool,
}

/// Common attributes to make the symbolicator skip/consider sources by certain criteria.
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SourceFilters {
    /// File types that are supported by this server.
    pub filetypes: Vec<FileType>,

    /// When nonempty, a list of glob patterns to fuzzy-match filepaths against. The source is then
    /// only used if one of the patterns matches.
    ///
    /// "Fuzzy" in this context means that (ascii) casing is ignored, and `\` is treated as equal
    /// to `/`.
    ///
    /// If a debug image does not contain any path information it will be treated like an image
    /// whose path doesn't match any pattern.
    pub path_patterns: Vec<Glob>,
}

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

/// Determines how files are named in an external source.
#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(default)]
pub struct DirectoryLayout {
    /// Directory layout of this symbol server.
    #[serde(rename = "type")]
    pub ty: DirectoryLayoutType,

    /// Overwrite filename casing convention of `self.layout`. This is useful in the case of
    /// DirectoryLayout::Symstore, where servers are supposed to handle requests
    /// case-insensitively, but practically don't (in the case of S3 buckets it's not possible),
    /// making this aspect not well-specified.
    pub casing: FilenameCasing,
}

impl Default for DirectoryLayout {
    fn default() -> DirectoryLayout {
        DirectoryLayout {
            ty: DirectoryLayoutType::Native,
            casing: Default::default(),
        }
    }
}

/// Known conventions for `DirectoryLayout`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum DirectoryLayoutType {
    /// Uses conventions of native debuggers.
    #[serde(rename = "native")]
    Native,
    /// Uses Microsoft symbol server conventions.
    #[serde(rename = "symstore")]
    Symstore,
    /// Uses Microsoft symbol server conventions (2 Tier Layout)
    #[serde(rename = "symstore_index2")]
    SymstoreIndex2,
    /// Uses Microsoft SSQP server conventions.
    #[serde(rename = "ssqp")]
    SSQP,
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
            SourceConfig::Gcs(ref x) => &x.id,
            SourceConfig::Sentry(ref x) => &x.id,
            SourceConfig::Filesystem(ref x) => &x.id,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match *self {
            SourceConfig::Sentry(..) => "sentry",
            SourceConfig::S3(..) => "s3",
            SourceConfig::Gcs(..) => "gcs",
            SourceConfig::Http(..) => "http",
            SourceConfig::Filesystem(..) => "filesystem",
        }
    }

    /// Determines whether debug files from this bucket may be shared.
    pub fn is_public(&self) -> bool {
        match *self {
            SourceConfig::Http(ref x) => x.files.is_public,
            SourceConfig::S3(ref x) => x.files.is_public,
            SourceConfig::Gcs(ref x) => x.files.is_public,
            SourceConfig::Sentry(_) => false,
            SourceConfig::Filesystem(ref x) => x.files.is_public,
        }
    }
}

impl WriteSentryScope for SourceConfig {
    fn write_sentry_scope(&self, scope: &mut sentry::Scope) {
        scope.set_tag("source.id", self.id());
        scope.set_tag("source.type", self.type_name());
        scope.set_tag("source.is_public", self.is_public());
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

/// A map of register values.
pub type Registers = BTreeMap<String, HexValue>;

/// An unsymbolicated frame from a symbolication request.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct RawFrame {
    /// The absolute instruction address of this frame.
    pub instruction_addr: HexValue,

    /// The path to the image this frame is located in.
    #[serde(default, skip_serializing_if = "is_default")]
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
    #[serde(default, skip_serializing_if = "is_default")]
    pub trust: FrameTrust,
}

#[derive(Clone, Debug, Default, Deserialize)]
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
    #[serde(default, skip_serializing_if = "is_default")]
    pub code_id: Option<String>,

    /// Name of the code file.
    #[serde(default, skip_serializing_if = "is_default")]
    pub code_file: Option<String>,

    /// Identifier of the debug file.
    #[serde(skip_serializing_if = "is_default")]
    pub debug_id: Option<String>,

    /// Name of the debug file.
    #[serde(default, skip_serializing_if = "is_default")]
    pub debug_file: Option<String>,

    /// Absolute address at which the image was mounted into virtual memory.
    pub image_addr: HexValue,

    /// Size of the image in virtual memory.
    #[serde(default, skip_serializing_if = "is_default")]
    pub image_size: Option<u64>,
}

/// The type of an object file.
#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct ObjectType(pub String);

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
    #[serde(skip_serializing_if = "is_default")]
    pub thread_id: Option<u64>,

    /// If a dump was produced as a result of a crash, this will point to the thread that crashed.
    /// If the dump was produced by user code without crashing, and the dump contains extended
    /// Breakpad information, this will point to the thread that requested the dump.
    ///
    /// Currently only `Some` for minidumps.
    #[serde(skip_serializing_if = "is_default")]
    pub is_requesting: Option<bool>,

    /// Registers, only useful when returning a processed minidump.
    #[serde(default, skip_serializing_if = "is_default")]
    pub registers: Registers,

    /// Frames of this stack trace.
    pub frames: Vec<SymbolicatedFrame>,
}

/// Information on a debug information file.
#[derive(Debug, Clone, Copy, Serialize, Eq, PartialEq)]
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

/// Normalized RawObjectInfo with status attached.
///
/// RawObjectInfo is what the user sends and CompleteObjectInfo is what the user gets.
#[derive(Debug, Clone, Serialize, Eq, PartialEq)]
pub struct CompleteObjectInfo {
    /// Status for fetching the file with debug info.
    pub debug_status: ObjectFileStatus,
    /// Status for fetching the file with unwind info (for minidump stackwalking).
    #[serde(skip_serializing_if = "is_default", default)]
    pub unwind_status: Option<ObjectFileStatus>,
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
            debug_status: Default::default(),
            unwind_status: Default::default(),
            arch: Default::default(),
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
    #[serde(skip_serializing_if = "is_default")]
    pub timestamp: Option<u64>,

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

    /// A detailed explanation of the crash, potentially in human readable form. This may
    /// include a string representation of the crash reason or application-specific info.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crash_details: Option<String>,

    /// If there was an assertion that was hit, a textual representation of that assertion,
    /// possibly including the file and line at which it occurred.
    #[serde(skip_serializing_if = "is_default")]
    pub assertion: Option<String>,

    /// The threads containing symbolicated stack frames.
    pub stacktraces: Vec<CompleteStacktrace>,

    /// A list of images, extended with status information.
    pub modules: Vec<CompleteObjectInfo>,
}

/// Information about the operating system.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
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

/// This type only exists to have a working impl of `Fail` for `Arc<T> where T: Fail`. We cannot
/// contribute a blanket impl upstream because it would conflict with at least this blanket impl
/// from failure: `impl<E: StdError + Send + Sync + 'static> Fail for E`
#[derive(Debug, Clone)]
pub struct ArcFail<T>(pub Arc<T>);

impl<T: Fail> Fail for ArcFail<T> {
    fn cause(&self) -> Option<&dyn Fail> {
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
    /// Source bundle
    SourceBundle,
}

impl FileType {
    /// Lists all available file types.
    #[inline]
    pub fn all() -> &'static [Self] {
        use FileType::*;
        &[
            Pdb,
            MachDebug,
            ElfDebug,
            Pe,
            MachCode,
            ElfCode,
            Breakpad,
            SourceBundle,
        ]
    }

    /// Source providing file types.
    #[inline]
    pub fn sources() -> &'static [Self] {
        &[FileType::SourceBundle]
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
            FileType::SourceBundle => "sourcebundle",
        }
    }
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
    }
}
