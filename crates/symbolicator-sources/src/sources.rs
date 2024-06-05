//! Download sources types and related implementations.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::filetype::FileType;
use crate::paths;
use crate::types::{Glob, ObjectId};

mod filesystem;
mod gcs;
mod http;
mod s3;
mod sentry;
pub use filesystem::*;
pub use gcs::*;
pub use http::*;
pub use s3::*;
pub use sentry::*;

/// An identifier for DIF sources.
///
/// This is essentially a newtype for a string.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct SourceId(pub(crate) String);

// For now we allow this to be unused, some tests use these already.
impl SourceId {
    /// Creates a new [`SourceId`].
    #[allow(unused)]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Deref the [`SourceId`] to a `&str`.
    #[allow(unused)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Configuration for an external source.
///
/// Sources provide the ability to download Download Information Files (DIF).
/// Their configuration is a combination of the location of the source plus any
/// required authentication etc.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    /// Local file system.
    Filesystem(Arc<FilesystemSourceConfig>),
    /// A google cloud storage bucket.
    Gcs(Arc<GcsSourceConfig>),
    /// Http server implementing the Microsoft Symbol Server protocol.
    Http(Arc<HttpSourceConfig>),
    /// Amazon S3 bucket containing symbols in a directory hierarchy.
    S3(Arc<S3SourceConfig>),
    /// Sentry debug files endpoint.
    Sentry(Arc<SentrySourceConfig>),
}

impl SourceConfig {
    /// The unique identifier of this source.
    pub fn id(&self) -> &SourceId {
        match self {
            Self::Filesystem(x) => &x.id,
            Self::Gcs(x) => &x.id,
            Self::Http(x) => &x.id,
            Self::S3(x) => &x.id,
            Self::Sentry(x) => &x.id,
        }
    }

    /// Name of this source.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Filesystem(..) => "filesystem",
            Self::Gcs(..) => "gcs",
            Self::Http(..) => "http",
            Self::S3(..) => "s3",
            Self::Sentry(..) => "sentry",
        }
    }
}

/// Common parameters for external filesystem-like buckets configured by users.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CommonSourceConfig {
    /// Influence whether this source will be selected
    pub filters: SourceFilters,

    /// How files are laid out in this storage.
    pub layout: DirectoryLayout,

    /// Whether debug files are shared across scopes.
    pub is_public: bool,
}

impl CommonSourceConfig {
    /// Creates a config with the given [`DirectoryLayoutType`]
    pub fn with_layout(layout_type: DirectoryLayoutType) -> Self {
        Self {
            layout: DirectoryLayout {
                ty: layout_type,
                ..DirectoryLayout::default()
            },
            ..Self::default()
        }
    }
}

/// Common attributes to make the symbolicator skip/consider sources by certain criteria.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
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

impl SourceFilters {
    /// Whether the [`ObjectId`] / [`FileType`] combination is allowed on this source.
    pub fn is_allowed(&self, object_id: &ObjectId, filetype: FileType) -> bool {
        (self.filetypes.is_empty() || self.filetypes.contains(&filetype))
            && paths::matches_path_patterns(object_id, &self.path_patterns)
    }
}

/// Determines how files are named in an external source.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct DirectoryLayout {
    /// Directory layout of this symbol server.
    #[serde(rename = "type")]
    pub ty: DirectoryLayoutType,

    /// Overwrite the default filename casing convention of the [layout type](Self::ty).
    ///
    /// This is useful in the case of [`DirectoryLayoutType::Symstore`], where servers are supposed to
    /// handle requests case-insensitively, but practically do not, making this aspect not
    /// well-specified. For instance, in S3 buckets it is not possible to perform case-insensitive
    /// queries.
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

/// Known conventions for [`DirectoryLayout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
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
    Ssqp,
    /// Uses [debuginfod](https://www.mankier.com/8/debuginfod) conventions.
    #[serde(rename = "debuginfod")]
    Debuginfod,
    /// Unified sentry proprietary bucket format.
    #[serde(rename = "unified")]
    Unified,
    /// A simple symbol source using the `{code_id}/symbols` as its search path.
    #[serde(rename = "slashsymbols")]
    SlashSymbols,
}

/// Casing of filenames on the symbol server
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum FilenameCasing {
    /// Default casing depending on layout type.
    #[default]
    Default,
    /// Uppercase filenames.
    Uppercase,
    /// Lowercase filenames.
    Lowercase,
}
