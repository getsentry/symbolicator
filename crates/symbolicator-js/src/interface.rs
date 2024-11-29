use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use symbolicator_service::caching::CacheError;
use symbolicator_service::types::{Platform, Scope, ScrapingConfig};
use symbolicator_sources::{SentryFileId, SentrySourceConfig};

use crate::lookup::CachedFileUri;

#[derive(Debug, Clone)]
pub struct SymbolicateJsStacktraces {
    /// The event's platform.
    ///
    /// `Platform` is actually too lenient of a type here—a legitimate
    /// JS event should only have
    /// [`JavaScript`](symbolicator_service::types::JsPlatform::JavaScript)
    /// or [`Node`](symbolicator_service::types::JsPlatform::Node). However,
    /// we use the general `Platform` type for now to be resilient against
    /// wrong values making it through.
    pub platform: Option<Platform>,
    pub scope: Scope,
    pub source: Arc<SentrySourceConfig>,
    pub release: Option<String>,
    pub dist: Option<String>,
    pub stacktraces: Vec<JsStacktrace>,
    pub modules: Vec<JsModule>,
    pub scraping: ScrapingConfig,
    /// Whether to apply source context for the stack frames.
    pub apply_source_context: bool,
}

// Some of the renames are there only to make it synchronized
// with the already existing monolith naming scheme.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type")]
pub enum JsModuleErrorKind {
    InvalidLocation { line: u32, col: Option<u32> },
    InvalidAbsPath,
    NoColumn,
    MissingSourceContent { source: String, sourcemap: String },
    MissingSource,
    MalformedSourcemap { url: String },
    MissingSourcemap,
    InvalidBase64Sourcemap,
    ScrapingDisabled,
}

impl fmt::Display for JsModuleErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsModuleErrorKind::InvalidLocation { line, col } => {
                write!(f, "Invalid source location")?;
                match (line, col) {
                    (l, None) => write!(f, ": line:{l}")?,
                    (l, Some(c)) => write!(f, ": line:{l}, col:{c}")?,
                }
                Ok(())
            }
            JsModuleErrorKind::InvalidAbsPath => write!(f, "Invalid absolute path"),
            JsModuleErrorKind::NoColumn => write!(f, "No column information"),
            JsModuleErrorKind::MissingSourceContent { source, sourcemap } => write!(
                f,
                "Missing source contents for source file {source} and sourcemap file {sourcemap}"
            ),
            JsModuleErrorKind::MissingSource => write!(f, "Missing source file"),
            JsModuleErrorKind::MalformedSourcemap { url } => {
                write!(f, "Sourcemap file at {url} is malformed")
            }
            JsModuleErrorKind::MissingSourcemap => write!(f, "Missing sourcemap file"),
            JsModuleErrorKind::InvalidBase64Sourcemap => write!(f, "Invalid base64 sourcemap"),
            JsModuleErrorKind::ScrapingDisabled => {
                write!(f, "Could not download file because scraping is disabled")
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct JsModuleError {
    pub abs_path: String,
    #[serde(flatten)]
    pub kind: JsModuleErrorKind,
}

/// A JS module (representing a minified file and sourcemap).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct JsModule {
    /// The type of the module.
    ///
    /// This field carries no useful information because `JsModuleType`
    /// only has one variant. It is used to distinguish `JsModule`s from
    /// JVM and native modules during deserialization.
    pub r#type: JsModuleType,
    /// The module's minified code file.
    pub code_file: String,
    /// The module's debug ID.
    pub debug_id: String,
}

/// The type of a sourcemap module.
///
/// As per <https://develop.sentry.dev/sdk/data-model/event-payloads/debugmeta/#source-map-images>,
/// this is always `"sourcemap"`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum JsModuleType {
    Sourcemap,
}

/// An attempt to scrape a JS source or sourcemap file from the web.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsScrapingAttempt {
    /// The URL we attempted to scrape from.
    pub url: String,
    /// The outcome of the attempt.
    #[serde(flatten)]
    pub result: JsScrapingResult,
}

impl JsScrapingAttempt {
    pub fn success(url: String) -> Self {
        Self {
            url,
            result: JsScrapingResult::Success,
        }
    }
    pub fn not_attempted(url: String) -> Self {
        Self {
            url,
            result: JsScrapingResult::NotAttempted,
        }
    }

    pub fn failure(url: String, reason: JsScrapingFailureReason, details: String) -> Self {
        Self {
            url,
            result: JsScrapingResult::Failure { reason, details },
        }
    }
}

/// The outcome of a scraping attempt.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "status")]
pub enum JsScrapingResult {
    /// We didn't actually attempt scraping because we already obtained the file
    /// by another method.
    NotAttempted,
    /// The file was succesfully scraped.
    Success,
    /// The file couldn't be scraped.
    Failure {
        /// The basic reason for the failure.
        reason: JsScrapingFailureReason,
        #[serde(skip_serializing_if = "String::is_empty")]
        /// A more detailed explanation of the failure.
        details: String,
    },
}

impl From<CacheError> for JsScrapingResult {
    fn from(value: CacheError) -> Self {
        let (reason, details) = match value {
            CacheError::NotFound => (JsScrapingFailureReason::NotFound, String::new()),
            CacheError::PermissionDenied(details) => {
                (JsScrapingFailureReason::PermissionDenied, details)
            }
            CacheError::Timeout(duration) => (
                JsScrapingFailureReason::Timeout,
                format!("Timeout after {}", humantime::format_duration(duration)),
            ),
            CacheError::DownloadError(details) => (JsScrapingFailureReason::DownloadError, details),
            CacheError::Malformed(details) => (JsScrapingFailureReason::Other, details),
            CacheError::Unsupported(details) => (JsScrapingFailureReason::Other, details),
            CacheError::InternalError => (JsScrapingFailureReason::Other, String::new()),
        };

        Self::Failure { reason, details }
    }
}

/// The basic reason a scraping attempt failed.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JsScrapingFailureReason {
    /// The file was not found at the given URL.
    NotFound,
    /// Scraping was disabled.
    Disabled,
    /// The URL was not in the list of allowed hosts or had
    /// an invalid scheme.
    InvalidHost,
    /// Permission to access the file was denied.
    PermissionDenied,
    /// The scraping attempt timed out.
    Timeout,
    /// There was a non-timeout error while downloading.
    DownloadError,
    /// Catchall case.
    ///
    /// This probably can't actually happen.
    Other,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JsFrame {
    /// The frame's platform.
    ///
    /// `Platform` is actually too lenient of a type here—a legitimate
    /// JS frame should only have
    /// [`JavaScript`](symbolicator_service::types::JsPlatform::JavaScript)
    /// or [`Node`](symbolicator_service::types::JsPlatform::Node). However,
    /// we use the general `Platform` type for now to be resilient against
    /// wrong values making it through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,

    pub abs_path: String,

    pub lineno: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub colno: Option<u32>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_context: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_line: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_context: Vec<String>,

    #[serde(skip_serializing)]
    pub token_name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_app: Option<bool>,

    #[serde(default)]
    pub data: JsFrameData,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JsFrameData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sourcemap: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sourcemap_origin: Option<CachedFileUri>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_with: Option<ResolvedWith>,
    #[serde(default)]
    pub symbolicated: bool,
}

/// A marker indicating what a File was resolved with.
///
/// This enum serves a double purpose, both marking how an individual file was found inside of a
/// bundle, as well as tracking through which method that bundle itself was found.
///
#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ResolvedWith {
    /// Both: Found in a Bundle via DebugId
    /// And: Found the Bundle via API Lookup via DebugId / Database Index
    DebugId,
    /// Found in a Bundle via Url matching
    Url,
    /// Found the Bundle via API Lookup via Database Index
    Index,
    /// Found the Bundle via API Lookup as an ArtifactBundle
    Release,
    /// Found the Bundle via API Lookup as a ReleaseFile
    ReleaseOld,
    /// Scraped the File from the Web
    Scraping,
    /// Unknown
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct JsStacktrace {
    pub frames: Vec<JsFrame>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct CompletedJsSymbolicationResponse {
    pub stacktraces: Vec<JsStacktrace>,
    pub raw_stacktraces: Vec<JsStacktrace>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<JsModuleError>,
    #[serde(skip_serializing_if = "HashSet::is_empty")]
    pub used_artifact_bundles: HashSet<SentryFileId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scraping_attempts: Vec<JsScrapingAttempt>,
}
