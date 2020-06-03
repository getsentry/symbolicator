use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Deserializer};
use url::Url;

use crate::types::{Glob, ObjectId, ObjectType};
use crate::utils::paths;
use crate::utils::sentry::WriteSentryScope;

/// Configuration for an external source.
///
/// Sources provide the ability to download Download Information Files (DIF).
/// Their configuration is a combination of the location of the source plus any
/// required authentication etc.
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

/// A location for a file retrievable from many source configs.
///
/// It is essentially a `/`-separated string.  This is currently used by all
/// sources other than [`SentrySourceConfig`].  This may change in the future.
///
/// [`SentrySourceConfig`]: struct.SentrySourceConfig.html
// TODO: make the field private.
#[derive(Debug, Clone)]
pub struct SourceLocation(pub String);

impl SourceLocation {
    /// Return an iterator of the location segments.
    pub fn segments<'a>(&'a self) -> impl std::iter::Iterator<Item = &str> + 'a {
        self.0.split('/').filter(|s| !s.is_empty())
    }
}

impl SourceLocation {
    pub fn new(loc: impl Into<String>) -> Self {
        SourceLocation(loc.into())
    }
}

impl fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An identifier for a file retrievable from a [`SentrySourceConfig`].
///
/// [`SentrySourceConfig`]: ../../types/struct.SentrySourceConfig.html
// TODO: make the field private.
#[derive(Debug, Clone)]
pub struct SentryFileId(pub String);

impl SentryFileId {
    pub fn new(id: impl Into<String>) -> Self {
        SentryFileId(id.into())
    }
}

impl fmt::Display for SentryFileId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
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
    pub files: CommonSourceConfig,
}

/// Configuration for reading from the local file system.
#[derive(Deserialize, Clone, Debug)]
pub struct FilesystemSourceConfig {
    /// Unique source identifier.
    pub id: String,

    /// Path to symbol directory.
    pub path: PathBuf,

    #[serde(flatten)]
    pub files: CommonSourceConfig,
}

impl FilesystemSourceConfig {
    pub fn join_loc(&self, loc: &SourceLocation) -> PathBuf {
        self.path.join(&loc.0)
    }
}

/// This uniquely identifies a file on a source and how to download it.
///
/// This is a combination of the [`SourceConfig`], which describes a download
/// source and how to download rom it, with an identifier describing a single
/// file in that source.
///
/// [`SourceConfig`]: enum.SourceConfig.html
#[derive(Debug, Clone)]
pub enum SourceFileId {
    Sentry(Arc<SentrySourceConfig>, SentryFileId),
    S3(Arc<S3SourceConfig>, SourceLocation),
    Gcs(Arc<GcsSourceConfig>, SourceLocation),
    Http(Arc<HttpSourceConfig>, SourceLocation),
    Filesystem(Arc<FilesystemSourceConfig>, SourceLocation),
}

/// Local helper to deserializes an S3 region string in `S3SourceKey`.
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
    pub files: CommonSourceConfig,
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
    pub files: CommonSourceConfig,
}

/// Common parameters for external filesystem-like buckets configured by users.
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct CommonSourceConfig {
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

impl SourceFilters {
    pub fn is_allowed(&self, object_id: &ObjectId, filetype: FileType) -> bool {
        (self.filetypes.is_empty() || self.filetypes.contains(&filetype))
            && paths::matches_path_patterns(object_id, &self.path_patterns)
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
    /// debuginfod conventions.
    #[serde(rename = "debuginfod")]
    Debuginfod,
    /// unified sentry propriertary bucket format
    #[serde(rename = "unified")]
    Unified,
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
    #[serde(rename = "sourcebundle")]
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
    pub fn from_object_type(ty: ObjectType) -> &'static [Self] {
        match ty {
            ObjectType::Macho => &[FileType::MachDebug, FileType::MachCode, FileType::Breakpad],
            ObjectType::Pe => &[FileType::Pdb, FileType::Pe, FileType::Breakpad],
            ObjectType::Elf => &[FileType::ElfDebug, FileType::ElfCode, FileType::Breakpad],
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
