//! Types for the Symbolicator API.
//!
//! This module contains some types which (de)serialise to/from JSON to make up the public
//! HTTP API.  Its messy and things probably need a better place and different way to signal
//! they are part of the public API.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use symbolicator_sources::ObjectType;

use crate::utils::hex::HexValue;

/// The scope of a source or debug file.
///
/// Based on scopes, access to debug files that have been cached is determined. If a file comes from
/// a public source, it can be used for any symbolication request. Otherwise, the symbolication
/// request must match the scope of a file.
#[derive(Debug, Clone, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd, Hash)]
#[serde(untagged)]
#[derive(Default)]
pub enum Scope {
    #[serde(rename = "global")]
    #[default]
    Global,
    Scoped(Arc<str>),
}

impl AsRef<str> for Scope {
    fn as_ref(&self) -> &str {
        match *self {
            Scope::Global => "global",
            Scope::Scoped(ref s) => s,
        }
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

/// Configuration for scraping of JS Sources, Source Maps and Source Context from the web.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScrapingConfig {
    /// Whether scraping should happen at all.
    pub enabled: bool,
    /// Whether Symbolicator should verify SSL certs when scraping from the web.
    ///
    /// Defaults to `true`, just to be safe.
    #[serde(default = "default_verify_ssl")]
    pub verify_ssl: bool,
    /// A list of "allowed origin patterns" that control:
    /// - for sourcemaps: what URLs we are allowed to scrape from.
    /// - for source context: which URLs should be authenticated using attached headers
    ///
    /// Allowed origins may be defined in several ways:
    /// - `http://domain.com[:port]`: Exact match for base URI (must include port).
    /// - `*`: Allow any domain.
    /// - `*.domain.com`: Matches domain.com and all subdomains, on any port.
    /// - `domain.com`: Matches domain.com on any port.
    /// - `*:port`: Wildcard on hostname, but explicit match on port.
    pub allowed_origins: Vec<String>,
    /// A map of headers to send with every HTTP request while scraping.
    pub headers: BTreeMap<String, String>,
}

impl Default for ScrapingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // verify_ssl: false,
            allowed_origins: vec!["*".to_string()],
            headers: Default::default(),
            verify_ssl: true,
        }
    }
}

fn default_verify_ssl() -> bool {
    true
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

    /// Checksum of the file's contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_checksum: Option<String>,

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

/// Information on a debug information file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ObjectFileStatus {
    /// The file was found and successfully processed.
    Found,
    /// The image was not referenced in the stack trace and not further handled.
    #[default]
    Unused,
    /// The file could not be found in any of the specified sources.
    Missing,
    /// The file failed to process.
    Malformed,
    /// The file could not be downloaded.
    FetchingFailed,
    /// Downloading or processing the file took too long.
    Timeout,
    /// The file could not be used for the purpose for which it was requested.
    ///
    /// This is currently only used when we try to symbolicate a .NET event with a Windows
    /// PDB file. A tracking issue in `symbolic` for supporting this case is
    /// [here](https://github.com/getsentry/symbolic/issues/871).
    Unsupported,
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
            ObjectFileStatus::Unsupported => "unsupported",
            ObjectFileStatus::Other => "other",
        }
    }
}

/// Possible values for the platform of a native event.
///
/// This corresponds to `NATIVE_PLATFORMS` in Sentry.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum NativePlatform {
    ObjC,
    Cocoa,
    Swift,
    Native,
    C,
    CSharp,
}

impl AsRef<str> for NativePlatform {
    fn as_ref(&self) -> &str {
        match self {
            NativePlatform::ObjC => "objc",
            NativePlatform::Cocoa => "cocoa",
            NativePlatform::Swift => "swift",
            NativePlatform::Native => "native",
            NativePlatform::C => "c",
            NativePlatform::CSharp => "csharp",
        }
    }
}

impl fmt::Display for NativePlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

/// Possible values for the platform of a JavaScript event.
///
/// This corresponds to the platforms listed in `is_js_event` in Sentry.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum JsPlatform {
    // Very rarely we see frames with a platform of `"nodejs"`.
    // We cover this case with an alias just to be sure.
    #[serde(alias = "nodejs")]
    Node,
    JavaScript,
}

impl AsRef<str> for JsPlatform {
    fn as_ref(&self) -> &str {
        match self {
            JsPlatform::Node => "node",
            JsPlatform::JavaScript => "javascript",
        }
    }
}

impl fmt::Display for JsPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

/// Possible values for the platform of a JVM event.
///
/// This covers `"java"` (used in error events) and `"android"` (used in
/// profiling).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum JvmPlatform {
    Java,
    Android,
}

impl AsRef<str> for JvmPlatform {
    fn as_ref(&self) -> &str {
        match self {
            JvmPlatform::Java => "java",
            JvmPlatform::Android => "android",
        }
    }
}

impl fmt::Display for JvmPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

/// Possible values for the platform of an event.
///
/// In addition to the native, JS, and JVM cases this also has a catch-all variant for
/// otherwise unrecognized platforms.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum Platform {
    Native(NativePlatform),
    Js(JsPlatform),
    Jvm(JvmPlatform),
    Other(String),
}

impl Platform {
    /// Returns `true` if this is a native platform.
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }

    /// Returns `true` if this is a JS platform.
    pub fn is_js(&self) -> bool {
        matches!(self, Self::Js(_))
    }

    /// Returns `true` if this is a JVM platform.
    pub fn is_jvm(&self) -> bool {
        matches!(self, Self::Jvm(_))
    }
}

impl Default for Platform {
    fn default() -> Self {
        Self::Other("unknown".to_owned())
    }
}

impl AsRef<str> for Platform {
    fn as_ref(&self) -> &str {
        match self {
            Platform::Native(p) => p.as_ref(),
            Platform::Js(p) => p.as_ref(),
            Platform::Jvm(p) => p.as_ref(),
            Platform::Other(p) => p.as_ref(),
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}
