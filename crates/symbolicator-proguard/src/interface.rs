use std::sync::Arc;
use std::{collections::HashMap, fmt};

use serde::{Deserialize, Serialize};
use symbolic::common::DebugId;
use symbolicator_service::types::{Platform, Scope};
use symbolicator_sources::SourceConfig;

/// A request for symbolication/remapping of a JVM event.
#[derive(Debug, Clone)]
pub struct SymbolicateJvmStacktraces {
    /// The event's platform.
    ///
    /// `Platform` is actually too lenient of a type here—a legitimate
    /// JVM event should only have
    /// [`Java`](symbolicator_service::types::JvmPlatform::Java). However,
    /// we use the general `Platform` type for now to be resilient against
    /// wrong values making it through.
    pub platform: Option<Platform>,
    /// The scope of this request which determines access to cached files.
    pub scope: Scope,
    /// A list of external sources to load debug files.
    pub sources: Arc<[SourceConfig]>,
    /// The exceptions to symbolicate/remap.
    pub exceptions: Vec<JvmException>,
    /// The list of stacktraces to symbolicate/remap.
    pub stacktraces: Vec<JvmStacktrace>,
    /// A list of proguard files to use for remapping.
    pub modules: Vec<JvmModule>,
    /// Whether to apply source context for the stack frames.
    pub apply_source_context: bool,
    /// The package name of the release this event belongs to.
    ///
    /// This is used to set a remapped frame's `in_app` field.
    pub release_package: Option<String>,
    /// An list of additional class names that should be remapped.
    pub classes: Vec<Arc<str>>,
}

/// A stack frame in a JVM stacktrace.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JvmFrame {
    /// The frame's platform.
    ///
    /// `Platform` is actually too lenient of a type here—a legitimate
    /// JVM frame should only have
    /// [`Java`](symbolicator_service::types::JvmPlatform::Java). However,
    /// we use the general `Platform` type for now to be resilient against
    /// wrong values making it through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,

    /// The frame's function name.
    ///
    /// For a JVM frame, this is always a class method.
    pub function: String,

    /// The source file name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,

    /// The frame's method name.
    ///
    /// For a JVM frame, this is a fully qualified class name.
    pub module: String,

    /// The source file's absolute path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abs_path: Option<String>,

    /// The line number within the source file, starting at `1` for the first line.
    ///
    /// If a frame doesn't have a line number, only its class name can be remapped,
    /// and source context won't work at all.
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

    /// Whether the frame is related to app-code (rather than libraries/dependencies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_app: Option<bool>,

    /// The index of the frame in the stacktrace before filtering and sending to Symbolicator.
    ///
    /// When returning frames in a `CompletedJvmSymbolicationResponse`, all frames that were
    /// expanded from the frame with index `i` will also have index `i`.
    pub index: usize,

    /// Method signature.
    ///
    /// This has to be set when we want to deobfuscate based on the function parameters
    /// instead of using line numbers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// An exception in a JVM event.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JvmException {
    /// The type (class name) of the exception.
    #[serde(rename = "type")]
    pub ty: String,
    /// The module in which the exception is defined.
    pub module: String,
}

/// A JVM stacktrace.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JvmStacktrace {
    /// The stacktrace's frames.
    pub frames: Vec<JvmFrame>,
}

/// A JVM module (source bundle or proguard file).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JvmModule {
    /// The file's UUID.
    ///
    /// This is used to download the file from symbol sources.
    pub uuid: DebugId,
    /// The file's type.
    pub r#type: JvmModuleType,
}

/// The type of a [`JvmModule`].
///
/// For JVM symbolication we only use proguard files and source bundles.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JvmModuleType {
    /// A source bundle.
    Source,
    /// A proguard mapping file.
    Proguard,
}

/// The type of a [`ProguardError`].
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProguardErrorKind {
    /// The file couldn't be downloaded.
    Missing,
    /// The file is invalid according to [`is_valid`](proguard::ProguardMapping::is_valid).
    Invalid,
    /// The file doesn't contain line mapping information.
    NoLineInfo,
}

impl fmt::Display for ProguardErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProguardErrorKind::Missing => write!(f, "The proguard mapping file is missing."),
            ProguardErrorKind::Invalid => write!(f, "The proguard mapping file is invalid."),
            ProguardErrorKind::NoLineInfo => {
                write!(f, "The proguard mapping file does not contain line info.")
            }
        }
    }
}

/// An error that happened when trying to use a proguard mapping file.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProguardError {
    /// The UUID of the proguard file.
    pub uuid: DebugId,
    /// The type of the error.
    #[serde(rename = "type")]
    pub kind: ProguardErrorKind,
}

/// The symbolicated/remapped event data.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CompletedJvmSymbolicationResponse {
    /// The exceptions after remapping.
    pub exceptions: Vec<JvmException>,
    /// The stacktraces after remapping.
    pub stacktraces: Vec<JvmStacktrace>,
    /// A mapping from obfuscated to remapped classes.
    pub classes: HashMap<Arc<str>, Arc<str>>,
    /// Errors that occurred during symbolication.
    pub errors: Vec<ProguardError>,
}
