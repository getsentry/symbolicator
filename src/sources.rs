//! Download sources types and related implementations.

use anyhow::{Error, Result};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use url::Url;

use crate::types::{Glob, ObjectId, ObjectType};
use crate::utils::paths;
use crate::utils::sentry::WriteSentryScope;

/// An identifier for DIF sources.
///
/// This is essentially a newtype for a string.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct SourceId(String);

// For now we allow this to be unused, some tests use these already.
impl SourceId {
    #[allow(unused)]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

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
    pub fn id(&self) -> &SourceId {
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
/// It is essentially a `/`-separated string. This is currently used by all sources other than
/// [`SentrySourceConfig`]. This may change in the future.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct SourceLocation(String);

impl SourceLocation {
    pub fn new(loc: impl Into<String>) -> Self {
        SourceLocation(loc.into())
    }

    /// Return an iterator of the location segments.
    pub fn segments<'a>(&'a self) -> impl Iterator<Item = &str> + 'a {
        self.0.split('/').filter(|s| !s.is_empty())
    }
}

impl fmt::Display for SourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An identifier for a file retrievable from a [`SentrySourceConfig`].
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize)]
pub struct SentryFileId(String);

impl SentryFileId {
    pub fn as_str(&self) -> &str {
        &self.0
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
    pub id: SourceId,

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
    pub id: SourceId,

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
    pub id: SourceId,

    /// Path to symbol directory.
    pub path: PathBuf,

    #[serde(flatten)]
    pub files: CommonSourceConfig,
}

/// Represents a single object file stored on a source.
///
/// This joins the file location together with a [`SourceConfig`] and thus provides all
/// information to retrieve the object file from its source.
#[derive(Debug, Clone)]
pub enum ObjectFileSource {
    Sentry(SentryObjectFileSource),
    Http(HttpObjectFileSource),
    S3(S3ObjectFileSource),
    Gcs(GcsObjectFileSource),
    Filesystem(FilesystemObjectFileSource),
}

impl fmt::Display for ObjectFileSource {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ObjectFileSource::Sentry(ref s) => {
                write!(f, "Sentry source '{}' file id '{}'", s.source.id, s.file_id)
            }
            ObjectFileSource::Http(ref s) => {
                write!(f, "HTTP source '{}' location '{}'", s.source.id, s.location)
            }
            ObjectFileSource::S3(ref s) => {
                write!(f, "S3 source '{}' location '{}'", s.source.id, s.location)
            }
            ObjectFileSource::Gcs(ref s) => {
                write!(f, "GCS source '{}' location '{}'", s.source.id, s.location)
            }
            ObjectFileSource::Filesystem(ref s) => {
                write!(
                    f,
                    "Filesystem source '{}' location '{}'",
                    s.source.id, s.location
                )
            }
        }
    }
}

impl ObjectFileSource {
    /// Whether debug files from this source may be shared.
    pub fn is_public(&self) -> bool {
        match self {
            ObjectFileSource::Sentry(_) => false,
            ObjectFileSource::Http(ref x) => x.source.files.is_public,
            ObjectFileSource::S3(ref x) => x.source.files.is_public,
            ObjectFileSource::Gcs(ref x) => x.source.files.is_public,
            ObjectFileSource::Filesystem(ref x) => x.source.files.is_public,
        }
    }

    pub fn cache_key(&self) -> String {
        match self {
            ObjectFileSource::Sentry(ref x) => {
                format!("{}.{}.sentryinternal", x.source.id, x.file_id)
            }
            ObjectFileSource::Http(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            ObjectFileSource::S3(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            ObjectFileSource::Gcs(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
            ObjectFileSource::Filesystem(ref x) => {
                format!("{}.{}", x.source.id, x.location)
            }
        }
    }

    /// Returns the ID of the source.
    ///
    /// Within one request these IDs should be unique, this includes any sources from the
    /// configuration which are available to all requests.
    pub fn source_id(&self) -> &SourceId {
        match self {
            ObjectFileSource::Sentry(ref x) => &x.source.id,
            ObjectFileSource::Http(ref x) => &x.source.id,
            ObjectFileSource::S3(ref x) => &x.source.id,
            ObjectFileSource::Gcs(ref x) => &x.source.id,
            ObjectFileSource::Filesystem(ref x) => &x.source.id,
        }
    }

    pub fn source_type_name(&self) -> &'static str {
        match *self {
            ObjectFileSource::Sentry(..) => "sentry",
            ObjectFileSource::S3(..) => "s3",
            ObjectFileSource::Gcs(..) => "gcs",
            ObjectFileSource::Http(..) => "http",
            ObjectFileSource::Filesystem(..) => "filesystem",
        }
    }

    /// The location of the file on the source, normalised for all sources.
    ///
    /// The location is a string who's content is dependent on the source itself, which can
    /// identify the file on that source.  The underlying type might vary for source types
    /// however this normalises these all to [`SourceLocation`].  If you working on the
    /// source itself you should prefer accessing the location directly so you use the
    /// specific type.
    pub fn location(&self) -> SourceLocation {
        match self {
            ObjectFileSource::Sentry(ref x) => SourceLocation::new(x.file_id.as_str()),
            ObjectFileSource::Http(ref x) => x.location.clone(),
            ObjectFileSource::S3(ref x) => x.location.clone(),
            ObjectFileSource::Gcs(ref x) => x.location.clone(),
            ObjectFileSource::Filesystem(ref x) => x.location.clone(),
        }
    }

    /// Returns a URI for the location of the object file.
    ///
    /// There is no guarantee about any format of this URI, for some sources it could be
    /// very abstract.  In general the source should try and producde a URI which can be
    /// used directly into the source-specific tooling.  E.g. for an HTTP source this would
    /// be an `http://` or `https://` URL, for AWS S3 it would be an `s3://` url etc.
    pub fn uri(&self) -> String {
        match self {
            ObjectFileSource::Sentry(ref file_source) => file_source.url().as_str().to_string(),
            ObjectFileSource::Http(ref file_source) => match file_source.url() {
                Ok(url) => url.as_str().to_string(),
                Err(_) => String::new(),
            },
            ObjectFileSource::S3(ref file_source) => file_source.uri(),
            ObjectFileSource::Gcs(ref file_source) => file_source.uri(),
            ObjectFileSource::Filesystem(ref file_source) => file_source.uri(),
        }
    }
}

impl WriteSentryScope for ObjectFileSource {
    fn write_sentry_scope(&self, scope: &mut ::sentry::Scope) {
        scope.set_tag("source.id", self.source_id());
        scope.set_tag("source.type", self.source_type_name());
        scope.set_tag("source.is_public", self.is_public());
    }
}

impl From<SentryObjectFileSource> for ObjectFileSource {
    fn from(source: SentryObjectFileSource) -> Self {
        Self::Sentry(source)
    }
}

impl From<HttpObjectFileSource> for ObjectFileSource {
    fn from(source: HttpObjectFileSource) -> Self {
        Self::Http(source)
    }
}

impl From<S3ObjectFileSource> for ObjectFileSource {
    fn from(source: S3ObjectFileSource) -> Self {
        Self::S3(source)
    }
}

impl From<GcsObjectFileSource> for ObjectFileSource {
    fn from(source: GcsObjectFileSource) -> Self {
        Self::Gcs(source)
    }
}

impl From<FilesystemObjectFileSource> for ObjectFileSource {
    fn from(source: FilesystemObjectFileSource) -> Self {
        Self::Filesystem(source)
    }
}

#[derive(Debug, Clone)]
pub struct SentryObjectFileSource {
    pub source: Arc<SentrySourceConfig>,
    pub file_id: SentryFileId,
}

impl SentryObjectFileSource {
    pub fn new(source: Arc<SentrySourceConfig>, file_id: SentryFileId) -> Self {
        Self { source, file_id }
    }

    /// Returns the URL from which to download this object file.
    pub fn url(&self) -> Url {
        let mut url = self.source.url.clone();
        url.query_pairs_mut().append_pair("id", &self.file_id.0);
        url
    }
}

#[derive(Debug, Clone)]
pub struct HttpObjectFileSource {
    pub source: Arc<HttpSourceConfig>,
    pub location: SourceLocation,
}

impl HttpObjectFileSource {
    pub fn new(source: Arc<HttpSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the URL from which to download this object file.
    pub fn url(&self) -> Result<Url> {
        join_url_encoded(&self.source.url, &self.location)
    }
}

#[derive(Debug, Clone)]
pub struct S3ObjectFileSource {
    pub source: Arc<S3SourceConfig>,
    pub location: SourceLocation,
}

impl S3ObjectFileSource {
    pub fn new(source: Arc<S3SourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the S3 key.
    ///
    /// This is equivalent to the pathname within the bucket.
    pub fn key(&self) -> String {
        join_prefix_location(&self.source.prefix, &self.location)
    }

    /// Returns the S3 bucket name.
    pub fn bucket(&self) -> String {
        self.source.bucket.clone()
    }

    /// Returns the `s3://` URI from which to download this object file.
    pub fn uri(&self) -> String {
        format!(
            "s3://{bucket}/{key}",
            bucket = &self.source.bucket,
            key = self.key()
        )
    }
}

#[derive(Debug, Clone)]
pub struct GcsObjectFileSource {
    pub source: Arc<GcsSourceConfig>,
    pub location: SourceLocation,
}

impl GcsObjectFileSource {
    pub fn new(source: Arc<GcsSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the S3 key.
    ///
    /// This is equivalent to the pathname within the bucket.
    pub fn key(&self) -> String {
        join_prefix_location(&self.source.prefix, &self.location)
    }

    /// Returns the `gs://` URI from which to download this object file.
    pub fn uri(&self) -> String {
        format!(
            "gs://{bucket}/{key}",
            bucket = &self.source.bucket,
            key = self.key()
        )
    }
}

#[derive(Debug, Clone)]
pub struct FilesystemObjectFileSource {
    pub source: Arc<FilesystemSourceConfig>,
    pub location: SourceLocation,
}

impl FilesystemObjectFileSource {
    pub fn new(source: Arc<FilesystemSourceConfig>, location: SourceLocation) -> Self {
        Self { source, location }
    }

    /// Returns the path from which to fetch this object file.
    pub fn path(&self) -> PathBuf {
        self.source.path.join(&self.location.0)
    }

    /// Returns the `file://` URI from which to fetch this object file.
    ///
    /// This is a quick-and-dirty approximation, not fully RFC8089-compliant.  E.g. we do
    /// not provide a hostname nor percent-encode.  Use this only for diagnostics and use
    /// [`FilesystemObjectFile::path`] if the actual file location is needed.
    pub fn uri(&self) -> String {
        format!("file:///{path}", path = self.path().display())
    }
}

/// Joins the relative path to the given URL.
///
/// As opposed to [`Url::join`], this only supports relative paths. Each segment of the path is
/// percent-encoded. Empty segments are skipped, for example, `foo//bar` is collapsed to `foo/bar`.
///
/// The base URL is treated as directory. If it does not end with a slash, then a slash is
/// automatically appended.
///
/// Returns `Err` if the URL is cannot-be-a-base.
fn join_url_encoded(base: &Url, path: &SourceLocation) -> Result<Url> {
    let mut joined = base.clone();
    joined
        .path_segments_mut()
        .map_err(|_| Error::msg("URL cannot-be-a-base"))?
        .pop_if_empty()
        .extend(path.segments());
    Ok(joined)
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
    pub id: SourceId,

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

fn join_prefix_location(prefix: &str, location: &SourceLocation) -> String {
    let trimmed = prefix.trim_matches(&['/'][..]);
    if trimmed.is_empty() {
        location.0.clone()
    } else {
        format!("{}/{}", trimmed, location.0)
    }
}

/// Configuration for S3 symbol buckets.
#[derive(Deserialize, Clone, Debug)]
pub struct S3SourceConfig {
    /// Unique source identifier.
    pub id: SourceId,

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

impl CommonSourceConfig {
    #[cfg(test)]
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
    /// Uses [debuginfod](https://www.mankier.com/8/debuginfod) conventions.
    #[serde(rename = "debuginfod")]
    Debuginfod,
    /// Unified sentry propriertary bucket format.
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
    /// A WASM debug file
    WasmDebug,
    /// A WASM code file
    WasmCode,
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
            WasmCode,
            WasmDebug,
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
            ObjectType::Wasm => &[FileType::WasmCode, FileType::WasmDebug],
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
            FileType::WasmDebug => "wasm_debug",
            FileType::WasmCode => "wasm_code",
            FileType::Breakpad => "breakpad",
            FileType::SourceBundle => "sourcebundle",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_prefix_location() {
        let key = join_prefix_location(&String::from(""), &SourceLocation::new("spam/ham"));
        assert_eq!(key, "spam/ham");

        let key = join_prefix_location(&String::from("eggs"), &SourceLocation::new("spam/ham"));
        assert_eq!(key, "eggs/spam/ham");

        let key = join_prefix_location(&String::from("/eggs/bacon/"), &SourceLocation::new("spam"));
        assert_eq!(key, "eggs/bacon/spam");

        let key = join_prefix_location(&String::from("//eggs//"), &SourceLocation::new("spam"));
        assert_eq!(key, "eggs/spam");
    }

    #[test]
    fn test_sentry_source_download_url() {
        let source = SentrySourceConfig {
            id: SourceId::new("test"),
            url: Url::parse("https://example.net/endpoint/").unwrap(),
            token: "token".into(),
        };
        let file_source =
            SentryObjectFileSource::new(Arc::new(source), SentryFileId("abc123".into()));
        let url = file_source.url();
        assert_eq!(url.as_str(), "https://example.net/endpoint/?id=abc123");
    }

    #[test]
    fn test_join_empty() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("")).unwrap();
        assert_eq!(joined, "https://example.org/base".parse().unwrap());
    }

    #[test]
    fn test_join_space() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo bar")).unwrap();
        assert_eq!(
            joined,
            "https://example.org/base/foo%20bar".parse().unwrap()
        );
    }

    #[test]
    fn test_join_multiple() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo/bar")).unwrap();
        assert_eq!(joined, "https://example.org/base/foo/bar".parse().unwrap());
    }

    #[test]
    fn test_join_trailing_slash() {
        let base = Url::parse("https://example.org/base/").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo")).unwrap();
        assert_eq!(joined, "https://example.org/base/foo".parse().unwrap());
    }

    #[test]
    fn test_join_leading_slash() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("/foo")).unwrap();
        assert_eq!(joined, "https://example.org/base/foo".parse().unwrap());
    }

    #[test]
    fn test_join_multi_slash() {
        let base = Url::parse("https://example.org/base").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo//bar")).unwrap();
        assert_eq!(joined, "https://example.org/base/foo/bar".parse().unwrap());
    }

    #[test]
    fn test_join_absolute() {
        let base = Url::parse("https://example.org/").unwrap();
        let joined = join_url_encoded(&base, &SourceLocation::new("foo")).unwrap();
        assert_eq!(joined, "https://example.org/foo".parse().unwrap());
    }
}
