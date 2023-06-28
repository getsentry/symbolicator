//! The main logic to lookup and fetch JS/SourceMap-related files.
//!
//! # Lookup Logic
//!
//! An API request will feed into our list of [`ArtifactBundle`]s and potential artifact candidates.
//!
//! A request to [`SourceMapLookup::get_module`] will then fetch the minified source file, and its
//! corresponding `SourceMap`, either by [`DebugId`], by a `Sourcemap` reference, or a
//! `sourceMappingURL` comment within that file.
//!
//! Each file will be looked up first inside of all the open [`ArtifactBundle`]s.
//! If the requested file has a [`DebugId`], the lookup will be performed based on that first,
//! falling back to other lookup methods.
//! A file without [`DebugId`] will be looked up by a number of candidate URLs, see
//! [`get_release_file_candidate_urls`]. It will be first looked up inside all the open
//! [`ArtifactBundle`]s, falling back to individual artifacts, doing another API request if
//! necessary.
//! If none of the methods is successful, it will fall back to trying to load the file directly
//! from the Web if the `allow_scraping` option is `true`.
//!
//! In an ideal situation, all the file requests would be served by a single API request and a
//! single [`ArtifactBundle`], using [`DebugId`]s. Legacy usage of individual artifact files
//! and web scraping should trend to `0` with time.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{self, Write};
use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;

use futures::future::BoxFuture;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symbolic::common::{ByteView, DebugId, SelfCell};
use symbolic::debuginfo::js::discover_sourcemaps_location;
use symbolic::debuginfo::sourcebundle::{
    SourceBundleDebugSession, SourceFileDescriptor, SourceFileType,
};
use symbolic::debuginfo::Object;
use symbolic::sourcemapcache::{SourceMapCache, SourceMapCacheWriter};
use symbolicator_sources::{
    HttpRemoteFile, ObjectType, RemoteFile, RemoteFileUri, SentrySourceConfig,
};
use tempfile::NamedTempFile;

use crate::caching::{
    CacheEntry, CacheError, CacheItemRequest, CacheKey, CacheKeyBuilder, CacheVersions, Cacher,
};
use crate::services::download::DownloadService;
use crate::services::objects::ObjectMetaHandle;
use crate::types::{JsStacktrace, ResolvedWith, Scope};
use crate::utils::http::is_valid_origin;

use super::caches::versions::SOURCEMAP_CACHE_VERSIONS;
use super::caches::{ByteViewString, SourceFilesCache};
use super::download::sentry::{ArtifactHeaders, JsLookupResult};
use super::objects::{ObjectHandle, ObjectsActor};
use super::sourcemap::SourceMapService;
use super::symbolication::SymbolicateJsStacktraces;

pub type OwnedSourceMapCache = SelfCell<ByteView<'static>, SourceMapCache<'static>>;

/// Configuration for scraping of JS sources and sourcemaps from the web.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScrapingConfig {
    /// Whether scraping should happen at all.
    pub enabled: bool,
    // TODO: Can we even use this?
    // pub verify_ssl: bool,
    /// A list of "allowed origin patterns" that control what URLs we are
    /// allowed to scrape from.
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
        }
    }
}

/// A JS-processing "Module".
///
/// This is basically a single file (identified by its `abs_path`), with some additional metadata
/// about it.
#[derive(Clone, Debug)]
pub struct SourceMapModule {
    /// The original `abs_path`.
    abs_path: String,
    /// The optional [`DebugId`] of this module.
    debug_id: Option<DebugId>,
    // TODO(sourcemap): errors that happened when processing this file
    /// A flag showing if we have already resolved the minified and sourcemap files.
    was_fetched: bool,
    /// The base url for fetching source files.
    source_file_base: Option<String>,
    /// The fetched minified JS file.
    // TODO(sourcemap): maybe this should not be public?
    pub minified_source: CachedFileEntry,
    /// The converted SourceMap.
    // TODO(sourcemap): maybe this should not be public?
    pub smcache: Option<CachedFileEntry<OwnedSourceMapCache>>,
}

impl SourceMapModule {
    fn new(abs_path: &str, debug_id: Option<DebugId>) -> Self {
        Self {
            abs_path: abs_path.to_owned(),
            debug_id,
            was_fetched: false,
            source_file_base: None,
            minified_source: CachedFileEntry::empty(),
            smcache: None,
        }
    }

    /// Creates a new [`FileKey`] for the `file_path` relative to this module
    pub fn source_file_key(&self, file_path: &str) -> Option<FileKey> {
        let base_url = self.source_file_base.as_ref()?;
        let url = join_paths(base_url, file_path);
        Some(FileKey::new_source(url))
    }

    /// The base url for fetching source files.
    pub fn source_file_base(&self) -> Option<&str> {
        self.source_file_base.as_deref()
    }
}

pub struct SourceMapLookup {
    /// This is a map from the raw `abs_path` as it appears in the event to a [`SourceMapModule`].
    modules_by_abs_path: HashMap<String, SourceMapModule>,

    /// Arbitrary source files keyed by their [`FileKey`].
    files_by_key: HashMap<FileKey, CachedFileEntry>,

    /// The [`ArtifactFetcher`] responsible for fetching artifacts, from bundles or as individual files.
    fetcher: ArtifactFetcher,
}

impl SourceMapLookup {
    pub fn new(service: SourceMapService, request: SymbolicateJsStacktraces) -> Self {
        let SourceMapService {
            objects,
            sourcefiles_cache,
            sourcemap_caches,
            download_svc,
        } = service;

        let SymbolicateJsStacktraces {
            scope,
            source,
            modules,
            scraping,
            release,
            dist,
            ..
        } = request;

        let mut modules_by_abs_path = HashMap::with_capacity(modules.len());
        for module in modules {
            if module.ty != ObjectType::SourceMap {
                // TODO(sourcemap): raise an error?
                continue;
            }
            let Some(code_file) = module.code_file.as_ref() else {
                // TODO(sourcemap): raise an error?
                continue;
            };

            let debug_id = match &module.debug_id {
                Some(id) => {
                    // TODO(sourcemap): raise an error?
                    id.parse().ok()
                }
                None => None,
            };

            let cached_module = SourceMapModule::new(code_file, debug_id);

            modules_by_abs_path.insert(code_file.to_owned(), cached_module);
        }

        let fetcher = ArtifactFetcher {
            objects,
            sourcefiles_cache,
            sourcemap_caches,
            download_svc,

            scope,
            source,

            release,
            dist,
            scraping,

            artifact_bundles: Default::default(),
            individual_artifacts: Default::default(),

            api_requests: 0,
            queried_artifacts: 0,
            fetched_artifacts: 0,
            queried_bundles: 0,
            scraped_files: 0,
            found_via_bundle_debugid: 0,
            found_via_bundle_url: 0,
            found_via_scraping: 0,
        };

        Self {
            modules_by_abs_path,
            files_by_key: Default::default(),
            fetcher,
        }
    }

    /// Prepares the modules for processing
    pub fn prepare_modules(&mut self, stacktraces: &mut [JsStacktrace]) {
        for stacktrace in stacktraces {
            for frame in &mut stacktrace.frames {
                // NOTE: some older JS SDK versions did not correctly strip a leading `async `
                // prefix from the `abs_path`, which we will work around here.
                if let Some(abs_path) = frame.abs_path.strip_prefix("async ") {
                    frame.abs_path = abs_path.to_owned();
                }
                let abs_path = &frame.abs_path;
                if self.modules_by_abs_path.contains_key(abs_path) {
                    continue;
                }
                let cached_module = SourceMapModule::new(abs_path, None);
                self.modules_by_abs_path
                    .insert(abs_path.to_owned(), cached_module);
            }
        }
    }

    /// Get the [`SourceMapModule`], which gives access to the `minified_source` and `smcache`.
    pub async fn get_module(&mut self, abs_path: &str) -> &SourceMapModule {
        // An `entry_by_ref` would be so nice
        let module = self
            .modules_by_abs_path
            .entry(abs_path.to_owned())
            .or_insert_with(|| SourceMapModule::new(abs_path, None));

        if module.was_fetched {
            return module;
        }
        module.was_fetched = true;

        // we can’t have a mutable `module` while calling `fetch_module` :-(
        let (minified_source, smcache) = self
            .fetcher
            .fetch_minified_and_sourcemap(module.abs_path.clone(), module.debug_id)
            .await;

        // We use the sourcemap url as the base. If that is not available because there is no
        // sourcemap url, or it is an for embedded sourcemap, we fall back to the minified file.
        let sourcemap_url = match &minified_source.entry {
            Ok(minified_source) => match minified_source.sourcemap_url.as_deref() {
                Some(SourceMapUrl::Remote(url)) => Some(url.clone()),
                _ => None,
            },
            Err(_) => None,
        };
        let source_file_base = sourcemap_url.unwrap_or(module.abs_path.clone());

        module.source_file_base = Some(source_file_base);
        module.minified_source = minified_source;
        module.smcache = smcache;

        module
    }

    /// Gets the source file based on its [`FileKey`].
    pub async fn get_source_file(&mut self, key: FileKey) -> &CachedFileEntry {
        if !self.files_by_key.contains_key(&key) {
            let file = self.fetcher.get_file(&key).await;
            self.files_by_key.insert(key.clone(), file);
        }
        self.files_by_key.get(&key).expect("we should have a file")
    }

    /// Records various metrics for this Event, such as number of API requests.
    pub fn record_metrics(&self) {
        self.fetcher.record_metrics();
    }
}

/// Joins the `right` path to the `base` path, taking care of our special `~/` prefix that is treated just
/// like an absolute url.
pub fn join_paths(base: &str, right: &str) -> String {
    if right.contains("://") || right.starts_with("webpack:") {
        return right.into();
    }

    let (scheme, rest) = base.split_once("://").unwrap_or(("file", base));

    let right = right.strip_prefix('~').unwrap_or(right);
    // the right path is absolute:
    if right.starts_with('/') {
        if scheme == "file" {
            return right.into();
        }
        // a leading `//` means we are skipping the hostname
        if let Some(right) = right.strip_prefix("//") {
            return format!("{scheme}://{right}");
        }
        let hostname = rest.split('/').next().unwrap_or(rest);
        return format!("{scheme}://{hostname}{right}");
    }

    let mut final_path = String::new();

    let mut left_iter = rest.split('/').peekable();
    // add the scheme/hostname
    if scheme != "file" {
        let hostname = left_iter.next().unwrap_or_default();
        write!(final_path, "{scheme}://{hostname}").unwrap();
    } else if left_iter.peek() == Some(&"") {
        // pop a leading `/`
        let _ = left_iter.next();
    }

    // pop the basename from the back
    let _ = left_iter.next_back();

    let mut segments: Vec<_> = left_iter.collect();
    let is_http = scheme == "http" || scheme == "https";
    let mut is_first_segment = true;
    for right_segment in right.split('/') {
        if right_segment == ".." && (segments.pop().is_some() || is_http) {
            continue;
        }
        if right_segment == "." && (is_http || is_first_segment) {
            continue;
        }
        is_first_segment = false;

        segments.push(right_segment);
    }

    for seg in segments {
        // FIXME: do we want to skip all the `.` fragments as well?
        if !seg.is_empty() {
            write!(final_path, "/{seg}").unwrap();
        }
    }
    final_path
}

/// A URL to a sourcemap file.
///
/// May either be a conventional URL or a data URL containing the sourcemap
/// encoded as BASE64.
#[derive(Clone, PartialEq)]
enum SourceMapUrl {
    Data(ByteViewString),
    Remote(String),
}

impl fmt::Debug for SourceMapUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data(_) => f.debug_tuple("Data").field(&"...").finish(),
            Self::Remote(url) => f.debug_tuple("Remote").field(&url).finish(),
        }
    }
}

impl SourceMapUrl {
    /// Parses a string into a [`SourceMapUrl`].
    ///
    /// If it starts with `"data:"`, it is parsed as a data-URL that is base64 or url-encoded.
    /// Otherwise, the string is joined to the `base` URL.
    fn parse_with_prefix(base: &str, url_string: &str) -> CacheEntry<Self> {
        if url_string.starts_with("data:") {
            let decoded = data_url::DataUrl::process(url_string)
                .map_err(|_| ())
                .and_then(|url| url.decode_to_vec().map_err(|_| ()))
                .and_then(|data| String::from_utf8(data.0).map_err(|_| ()))
                .map_err(|_| CacheError::Malformed(String::from("invalid `data:` url")))?;

            Ok(Self::Data(decoded.into()))
        } else {
            let url = join_paths(base, url_string);
            Ok(Self::Remote(url))
        }
    }
}

type ArtifactBundle = SelfCell<Arc<ObjectHandle>, SourceBundleDebugSession<'static>>;

/// The lookup key of an arbitrary file.
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub enum FileKey {
    /// This key represents a [`SourceFileType::MinifiedSource`].
    MinifiedSource {
        abs_path: String,
        debug_id: Option<DebugId>,
    },
    /// This key represents a [`SourceFileType::SourceMap`].
    SourceMap {
        abs_path: Option<String>,
        debug_id: Option<DebugId>,
    },
    /// This key represents a [`SourceFileType::Source`].
    Source { abs_path: String },
}

impl FileKey {
    /// Creates a new [`FileKey`] for a source file.
    fn new_source(abs_path: String) -> Self {
        Self::Source { abs_path }
    }

    /// Returns this key's debug id, if any.
    fn debug_id(&self) -> Option<DebugId> {
        match self {
            FileKey::MinifiedSource { debug_id, .. } => *debug_id,
            FileKey::SourceMap { debug_id, .. } => *debug_id,
            FileKey::Source { .. } => None,
        }
    }

    /// Returns this key's abs_path, if any.
    pub fn abs_path(&self) -> Option<&str> {
        match self {
            FileKey::MinifiedSource { abs_path, .. } => Some(&abs_path[..]),
            FileKey::SourceMap { abs_path, .. } => abs_path.as_deref(),
            FileKey::Source { abs_path } => Some(&abs_path[..]),
        }
    }

    /// Returns the type of the file this key represents.
    fn as_type(&self) -> SourceFileType {
        match self {
            FileKey::MinifiedSource { .. } => SourceFileType::MinifiedSource,
            FileKey::SourceMap { .. } => SourceFileType::SourceMap,
            FileKey::Source { .. } => SourceFileType::Source,
        }
    }
}

/// The source of an individual file.
#[derive(Clone, Debug)]
pub enum CachedFileUri {
    /// The file was an individual artifact fetched using its own URI.
    IndividualFile(RemoteFileUri),
    /// The file was scraped from the web using the given URI.
    ScrapedFile(RemoteFileUri),
    /// The file was found using [`FileKey`] in the bundle identified by the URI.
    Bundled(RemoteFileUri, FileKey),
    /// The file was embedded in another file. This will only ever happen
    /// for Base64-encoded SourceMaps, and the SourceMap is always used
    /// in combination with a minified File that has a [`CachedFileUri`] itself.
    Embedded,
}

impl fmt::Display for CachedFileUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CachedFileUri::IndividualFile(uri) => write!(f, "{uri}"),
            CachedFileUri::ScrapedFile(uri) => write!(f, "{uri}"),
            CachedFileUri::Bundled(uri, key) => {
                write!(f, "{uri} / {:?}:", key.as_type())?;
                if let Some(abs_path) = key.abs_path() {
                    write!(f, "{abs_path}")?;
                } else {
                    write!(f, "-")?;
                }
                write!(f, " / ")?;
                if let Some(debug_id) = key.debug_id() {
                    write!(f, "{debug_id}")
                } else {
                    write!(f, "-")
                }
            }
            CachedFileUri::Embedded => f.write_str("<embedded>"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CachedFileEntry<T = CachedFile> {
    pub uri: CachedFileUri,
    pub entry: CacheEntry<T>,
    pub resolved_with: Option<ResolvedWith>,
}

impl<T> CachedFileEntry<T> {
    fn empty() -> Self {
        let uri = RemoteFileUri::new("<invalid>");
        Self {
            uri: CachedFileUri::IndividualFile(uri),
            entry: Err(CacheError::NotFound),
            resolved_with: None,
        }
    }
}

/// This is very similar to `SourceFileDescriptor`, except that it is `'static` and includes just
/// the parts that we care about.
#[derive(Clone)]
pub struct CachedFile {
    pub contents: ByteViewString,
    sourcemap_url: Option<Arc<SourceMapUrl>>,
}

impl fmt::Debug for CachedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let contents = if self.contents.len() > 64 {
            // its just `Debug` prints, but we would like the end of the file, as it may
            // have a `sourceMappingURL`
            format!("...{}", &self.contents[self.contents.len() - 61..])
        } else {
            self.contents.to_string()
        };
        f.debug_struct("CachedFile")
            .field("contents", &contents)
            .field("sourcemap_url", &self.sourcemap_url)
            .finish()
    }
}

impl CachedFile {
    fn from_descriptor(
        abs_path: Option<&str>,
        descriptor: SourceFileDescriptor,
    ) -> CacheEntry<Self> {
        let sourcemap_url = match descriptor.source_mapping_url() {
            Some(url) => {
                // `abs_path` here is expected to be the *absolute* `descriptor.url()`
                // The only case where `abs_path` is `None` is if we were looking up a `SourceMap` with
                // *just* the `DebugId`. But a `SourceMap` itself will never have a `source_mapping_url`, so we are good.
                let abs_path = abs_path.ok_or_else(|| {
                    CacheError::Malformed("using a descriptor without an `abs_path`".into())
                })?;
                Some(SourceMapUrl::parse_with_prefix(abs_path, url)?)
            }
            None => None,
        };

        let contents = descriptor
            .into_contents()
            .ok_or_else(|| CacheError::Malformed("descriptor should have `contents`".into()))?
            .into_owned();
        let contents = ByteViewString::from(contents);

        Ok(Self {
            contents,
            sourcemap_url: sourcemap_url.map(Arc::new),
        })
    }

    /// Returns a string representation of a SourceMap URL if it was coming from a remote resource.
    pub fn sourcemap_url(&self) -> Option<String> {
        self.sourcemap_url
            .as_ref()
            .and_then(|sm| match sm.as_ref() {
                SourceMapUrl::Remote(url) => Some(url.to_string()),
                SourceMapUrl::Data(_) => None,
            })
    }
}

#[derive(Debug)]
struct IndividualArtifact {
    remote_file: RemoteFile,
    headers: ArtifactHeaders,
    resolved_with: Option<ResolvedWith>,
}

struct ArtifactFetcher {
    // other services:
    objects: ObjectsActor,
    sourcefiles_cache: Arc<SourceFilesCache>,
    sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
    download_svc: Arc<DownloadService>,

    // source config
    scope: Scope,
    source: Arc<SentrySourceConfig>,

    // settings:
    release: Option<String>,
    dist: Option<String>,
    scraping: ScrapingConfig,

    /// The set of all the artifact bundles that we have downloaded so far.
    artifact_bundles: HashMap<RemoteFileUri, CacheEntry<(ArtifactBundle, Option<ResolvedWith>)>>,
    /// The set of individual artifacts, by their `url`.
    individual_artifacts: HashMap<String, IndividualArtifact>,

    // various metrics:
    api_requests: u64,
    queried_artifacts: u64,
    fetched_artifacts: u64,
    queried_bundles: u64,
    scraped_files: u64,
    found_via_bundle_url: i64,
    found_via_bundle_debugid: i64,
    found_via_scraping: i64,
}

impl ArtifactFetcher {
    /// Fetches the minified file, and the corresponding [`OwnedSourceMapCache`] for the file
    /// identified by its `abs_path`, or optionally its [`DebugId`].
    #[tracing::instrument(skip(self, abs_path), fields(%abs_path))]
    async fn fetch_minified_and_sourcemap(
        &mut self,
        abs_path: String,
        debug_id: Option<DebugId>,
    ) -> (
        CachedFileEntry,
        Option<CachedFileEntry<OwnedSourceMapCache>>,
    ) {
        // First, check if we have already cached / created the `SourceMapCache`.
        let key = FileKey::MinifiedSource { abs_path, debug_id };

        // Fetch the minified file first
        let minified_source = self.get_file(&key).await;

        // Then fetch the corresponding sourcemap if we have a sourcemap reference
        let sourcemap_url = match &minified_source.entry {
            Ok(minified_source) => minified_source.sourcemap_url.as_deref(),
            Err(_) => None,
        };

        // If we don't have sourcemap reference, nor a `DebugId`, we skip creating `SourceMapCache`.
        if sourcemap_url.is_none() && debug_id.is_none() {
            return (minified_source, None);
        }

        // We have three cases here:
        let sourcemap = match sourcemap_url {
            // We have an embedded SourceMap via data URL
            Some(SourceMapUrl::Data(data)) => CachedFileEntry {
                uri: CachedFileUri::Embedded,
                entry: Ok(CachedFile {
                    contents: data.clone(),
                    sourcemap_url: None,
                }),
                resolved_with: minified_source.resolved_with,
            },
            // We do have a valid `sourceMappingURL`
            Some(SourceMapUrl::Remote(url)) => {
                let sourcemap_key = FileKey::SourceMap {
                    abs_path: Some(url.clone()),
                    debug_id,
                };
                self.get_file(&sourcemap_key).await
            }
            // We may have a `DebugId`, in which case we don’t need no URL
            None => {
                let sourcemap_key = FileKey::SourceMap {
                    abs_path: None,
                    debug_id,
                };
                self.get_file(&sourcemap_key).await
            }
        };

        // Now that we (may) have both files, we can create a `SourceMapCache` for it
        let smcache = self
            .fetch_sourcemap_cache(&minified_source, sourcemap)
            .await;

        (minified_source, Some(smcache))
    }

    /// Fetches an arbitrary file using its `abs_path`,
    /// or optionally its [`DebugId`] and [`SourceFileType`]
    /// (because multiple files can share one [`DebugId`]).
    #[tracing::instrument(skip(self))]
    pub async fn get_file(&mut self, key: &FileKey) -> CachedFileEntry {
        // Try looking up the file in one of the artifact bundles that we know about.
        if let Some(file) = self.try_get_file_from_bundles(key) {
            return file;
        }

        // Otherwise, try to get the file from an individual artifact.
        if let Some(file) = self.try_fetch_file_from_artifacts(key).await {
            return file;
        }

        // Otherwise: Do a (cached) API lookup for the `abs_path` + `DebugId`
        if self.query_sentry_for_file(key).await {
            // At this point, *one* of our known artifacts includes the file we are looking for.
            // So we do the whole dance yet again.
            if let Some(file) = self.try_get_file_from_bundles(key) {
                return file;
            }
            if let Some(file) = self.try_fetch_file_from_artifacts(key).await {
                return file;
            }
        }

        // Otherwise, fall back to scraping from the Web.
        self.scrape(key).await
    }

    /// Attempt to scrape a file from the web.
    async fn scrape(&mut self, key: &FileKey) -> CachedFileEntry {
        let Some(abs_path) = key.abs_path() else {
            return CachedFileEntry::empty();
        };

        let url = match Url::parse(abs_path) {
            Ok(url) => url,
            Err(err) => {
                return CachedFileEntry {
                    uri: CachedFileUri::ScrapedFile(RemoteFileUri::new(abs_path)),
                    entry: Err(CacheError::DownloadError(err.to_string())),
                    resolved_with: None,
                }
            }
        };

        if !self.scraping.enabled {
            return CachedFileEntry {
                uri: CachedFileUri::ScrapedFile(RemoteFileUri::new(abs_path)),
                entry: Err(CacheError::DownloadError("Scraping disabled".to_string())),
                resolved_with: None,
            };
        }

        // Only scrape from http sources
        let scheme = url.scheme();
        if !["http", "https"].contains(&scheme) {
            return CachedFileEntry {
                uri: CachedFileUri::ScrapedFile(RemoteFileUri::new(abs_path)),
                entry: Err(CacheError::DownloadError(format!(
                    "{scheme} is not an allowed download scheme"
                ))),
                resolved_with: None,
            };
        }

        if !is_valid_origin(&url, &self.scraping.allowed_origins) {
            return CachedFileEntry {
                uri: CachedFileUri::ScrapedFile(RemoteFileUri::new(abs_path)),
                entry: Err(CacheError::DownloadError(format!(
                    "{abs_path} is not an allowed download origin"
                ))),
                resolved_with: None,
            };
        }

        let host_string = match url.host_str() {
            None => {
                return CachedFileEntry {
                    uri: CachedFileUri::ScrapedFile(RemoteFileUri::new(abs_path)),
                    entry: Err(CacheError::DownloadError("Invalid host".to_string())),
                    resolved_with: None,
                }
            }
            Some(host @ ("localhost" | "127.0.0.1")) => {
                if self.download_svc.can_connect_to_reserved_ips() {
                    host
                } else {
                    return CachedFileEntry {
                        uri: CachedFileUri::ScrapedFile(RemoteFileUri::new(abs_path)),
                        entry: Err(CacheError::DownloadError("Invalid host".to_string())),
                        resolved_with: None,
                    };
                }
            }
            Some(host) => host,
        };

        self.scraped_files += 1;

        let span = sentry::configure_scope(|scope| scope.get_span());
        let ctx = sentry::TransactionContext::continue_from_span(
            "scrape_js_file",
            "scrape_js_file",
            span,
        );
        let transaction = sentry::start_transaction(ctx);
        sentry::configure_scope(|scope| {
            scope.set_span(Some(transaction.clone().into()));
            scope.set_tag("host", host_string);
        });

        let mut remote_file = HttpRemoteFile::from_url(url.to_owned());
        remote_file.headers.extend(
            self.scraping
                .headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        let remote_file: RemoteFile = remote_file.into();
        let uri = CachedFileUri::ScrapedFile(remote_file.uri());
        let scraped_file = self
            .sourcefiles_cache
            .fetch_file(&self.scope, remote_file)
            .await;

        transaction.finish();
        CachedFileEntry {
            uri,
            entry: scraped_file.map(|contents| {
                self.found_via_scraping += 1;
                tracing::trace!(?key, "Found file by scraping the web");

                let sourcemap_url = discover_sourcemaps_location(&contents)
                    .and_then(|sm_ref| SourceMapUrl::parse_with_prefix(abs_path, sm_ref).ok())
                    .map(Arc::new);

                CachedFile {
                    contents,
                    sourcemap_url,
                }
            }),
            resolved_with: Some(ResolvedWith::Scraping),
        }
    }

    fn try_get_file_from_bundles(&mut self, key: &FileKey) -> Option<CachedFileEntry> {
        if self.artifact_bundles.is_empty() {
            return None;
        }

        // If we have a `DebugId`, we try a lookup based on that.
        if let Some(debug_id) = key.debug_id() {
            let ty = key.as_type();
            for (bundle_uri, bundle) in &self.artifact_bundles {
                let Ok((bundle, _)) = bundle else { continue; };
                let bundle = bundle.get();
                if let Ok(Some(descriptor)) = bundle.source_by_debug_id(debug_id, ty) {
                    self.found_via_bundle_debugid += 1;
                    tracing::trace!(?key, "Found file in artifact bundles by debug-id");
                    return Some(CachedFileEntry {
                        uri: CachedFileUri::Bundled(bundle_uri.clone(), key.clone()),
                        entry: CachedFile::from_descriptor(key.abs_path(), descriptor),
                        resolved_with: Some(ResolvedWith::DebugId),
                    });
                }
            }
        }

        // Otherwise, try all the candidate `abs_path` patterns in every artifact bundle.
        if let Some(abs_path) = key.abs_path() {
            for url in get_release_file_candidate_urls(abs_path) {
                for (bundle_uri, bundle) in &self.artifact_bundles {
                    let Ok((bundle, resolved_with)) = bundle else { continue; };
                    let bundle = bundle.get();
                    if let Ok(Some(descriptor)) = bundle.source_by_url(&url) {
                        self.found_via_bundle_url += 1;
                        tracing::trace!(?key, url, "Found file in artifact bundles by url");
                        return Some(CachedFileEntry {
                            uri: CachedFileUri::Bundled(bundle_uri.clone(), key.clone()),
                            entry: CachedFile::from_descriptor(Some(abs_path), descriptor),
                            resolved_with: *resolved_with,
                        });
                    }
                }
            }
        }

        None
    }

    async fn try_fetch_file_from_artifacts(&mut self, key: &FileKey) -> Option<CachedFileEntry> {
        if self.individual_artifacts.is_empty() {
            return None;
        }

        let abs_path = key.abs_path()?;
        let (url, artifact) = get_release_file_candidate_urls(abs_path).find_map(|url| {
            self.individual_artifacts
                .get(&url)
                .map(|artifact| (url, artifact))
        })?;

        // NOTE: we have no separate `found_via_artifacts` metric, as we don't expect these to ever
        // error, so one can use the `sum` of this metric:
        self.fetched_artifacts += 1;

        let mut artifact_contents = self
            .sourcefiles_cache
            .fetch_file(&self.scope, artifact.remote_file.clone())
            .await;

        if artifact_contents == Err(CacheError::NotFound) && !artifact.headers.is_empty() {
            // We save (React Native) Hermes Bytecode files as empty 0-size files,
            // in order to explicitly avoid applying any minified source-context from it.
            // However the symbolicator cache layer treats 0-size files as `NotFound`.
            // Work around that by reverting to an empty file on `NotFound`. As we are
            // dealing with Sentry API-provided artifacts, we *do* expect these to be found.
            artifact_contents = Ok(ByteViewString::from(String::new()));
        }

        Some(CachedFileEntry {
            uri: CachedFileUri::IndividualFile(artifact.remote_file.uri()),
            entry: artifact_contents.map(|contents| {
                tracing::trace!(?key, ?url, ?artifact, "Found file as individual artifact");

                // Get the sourcemap reference from the artifact, either from metadata, or file contents
                let sourcemap_url = resolve_sourcemap_url(abs_path, &artifact.headers, &contents);
                CachedFile {
                    contents,
                    sourcemap_url: sourcemap_url.map(Arc::new),
                }
            }),
            resolved_with: artifact.resolved_with,
        })
    }

    /// Queries the Sentry API for a single file (by its [`DebugId`] and file stem).
    ///
    /// Returns `true` if any new data was made available through this API request.
    async fn query_sentry_for_file(&mut self, key: &FileKey) -> bool {
        let mut debug_ids = BTreeSet::new();
        let mut file_stems = BTreeSet::new();
        if let Some(debug_id) = key.debug_id() {
            debug_ids.insert(debug_id);
        }
        if let Some(url) = key.abs_path() {
            let stem = extract_file_stem(url);
            file_stems.insert(stem);
        }

        self.query_sentry_for_files(debug_ids, file_stems).await
    }

    /// Queries the Sentry API for a bunch of [`DebugId`]s and file stems.
    ///
    /// This will also download all referenced [`ArtifactBundle`]s directly and persist them for
    /// later access.
    /// Individual files are not eagerly downloaded, but their metadata will be available.
    ///
    /// Returns `true` if any new data was made available through this API request.
    async fn query_sentry_for_files(
        &mut self,
        debug_ids: BTreeSet<DebugId>,
        file_stems: BTreeSet<String>,
    ) -> bool {
        if debug_ids.is_empty() {
            // `file_stems` only make sense in combination with a `release`.
            if file_stems.is_empty() || self.release.is_none() {
                // FIXME: this should really not happen, but I just observed it.
                // The callers should better validate the args in that case?
                return false;
            }
        }
        self.api_requests += 1;

        let results = match self
            .download_svc
            .lookup_js_artifacts(
                self.source.clone(),
                debug_ids,
                file_stems,
                self.release.as_deref(),
                self.dist.as_deref(),
            )
            .await
        {
            Ok(results) => results,
            Err(_err) => {
                // TODO(sourcemap): handle errors
                return false;
            }
        };

        let mut did_get_new_data = false;

        for file in results {
            match file {
                JsLookupResult::IndividualArtifact {
                    remote_file,
                    abs_path,
                    headers,
                    resolved_with,
                } => {
                    self.queried_artifacts += 1;
                    self.individual_artifacts
                        .entry(abs_path)
                        .or_insert_with(|| {
                            did_get_new_data = true;

                            // lowercase all the header keys
                            let headers = headers
                                .into_iter()
                                .map(|(k, v)| (k.to_lowercase(), v))
                                .collect();

                            IndividualArtifact {
                                remote_file,
                                headers,
                                resolved_with,
                            }
                        });
                }
                JsLookupResult::ArtifactBundle {
                    remote_file,
                    resolved_with,
                } => {
                    self.queried_bundles += 1;
                    let uri = remote_file.uri();
                    // clippy, you are wrong, as this would result in borrowing errors,
                    // because we are calling a `self` method while borrowing from self
                    #[allow(clippy::map_entry)]
                    if !self.artifact_bundles.contains_key(&uri) {
                        // NOTE: This could potentially be done concurrently, but lets not
                        // prematurely optimize for now
                        let artifact_bundle = self.fetch_artifact_bundle(remote_file).await;
                        self.artifact_bundles
                            .insert(uri, artifact_bundle.map(|bundle| (bundle, resolved_with)));

                        did_get_new_data = true;
                    }
                }
            }
        }

        did_get_new_data
    }

    #[tracing::instrument(skip(self))]
    async fn fetch_artifact_bundle(&self, file: RemoteFile) -> CacheEntry<ArtifactBundle> {
        let object_handle = ObjectMetaHandle::for_scoped_file(self.scope.clone(), file);

        let fetched_bundle = self.objects.fetch(object_handle).await?;

        SelfCell::try_new(fetched_bundle, |handle| unsafe {
            match (*handle).object() {
                Object::SourceBundle(source_bundle) => source_bundle
                    .debug_session()
                    .map_err(CacheError::from_std_error),
                obj => {
                    tracing::error!("expected a `SourceBundle`, got `{}`", obj.file_format());
                    Err(CacheError::InternalError)
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    async fn fetch_sourcemap_cache(
        &self,
        source: &CachedFileEntry,
        sourcemap: CachedFileEntry,
    ) -> CachedFileEntry<OwnedSourceMapCache> {
        fn write_cache_key(
            cache_key: &mut CacheKeyBuilder,
            prefix: &str,
            uri: &CachedFileUri,
            contents: &[u8],
        ) {
            if matches!(uri, CachedFileUri::ScrapedFile(_)) {
                let hash = Sha256::digest(contents);
                write!(cache_key, "{prefix}:\n{hash:x}\n").unwrap();
            } else {
                // TODO: using the `uri` here means we avoid an expensive hash calculation.
                // But it also means that a file that does not change but is included in
                // multiple bundles will cause the `SourceMapCache` to be regenerated.
                // We could potentially optimize this further by also keeping track of which
                // part of the `FileKey` was found in a bundle. If it was found via `DebugId`,
                // we could use that as a stable cache key, otherwise falling back on either
                // using the bundle URI+abs_path, or hashing the contents, depending on which
                // one is cheaper.
                write!(cache_key, "{prefix}:\n{uri}\n").unwrap();
            }
        }

        let smcache = match &source.entry {
            Ok(source_entry) => match sourcemap.entry {
                Ok(sourcemap_entry) => {
                    let source_content = source_entry.contents.clone();
                    let sourcemap_content = sourcemap_entry.contents;

                    let cache_key = {
                        let mut cache_key = CacheKey::scoped_builder(&self.scope);

                        write_cache_key(
                            &mut cache_key,
                            "source",
                            &source.uri,
                            source_content.as_ref(),
                        );
                        write_cache_key(
                            &mut cache_key,
                            "sourcemap",
                            &sourcemap.uri,
                            sourcemap_content.as_ref(),
                        );

                        cache_key.build()
                    };

                    let req = FetchSourceMapCacheInternal {
                        source: source_content,
                        sourcemap: sourcemap_content,
                    };
                    self.sourcemap_caches.compute_memoized(req, cache_key).await
                }
                Err(err) => Err(err),
            },
            Err(err) => Err(err.clone()),
        };
        CachedFileEntry {
            uri: sourcemap.uri,
            entry: smcache,
            resolved_with: sourcemap.resolved_with,
        }
    }

    fn record_metrics(&self) {
        metric!(time_raw("js.api_requests") = self.api_requests);
        metric!(time_raw("js.queried_bundles") = self.queried_bundles);
        metric!(time_raw("js.fetched_bundles") = self.artifact_bundles.len() as u64);
        metric!(time_raw("js.queried_artifacts") = self.queried_artifacts);
        metric!(time_raw("js.fetched_artifacts") = self.fetched_artifacts);
        metric!(time_raw("js.scraped_files") = self.scraped_files);
        metric!(counter("js.found_via_bundle_debugid") += self.found_via_bundle_debugid);
        metric!(counter("js.found_via_bundle_url") += self.found_via_bundle_url);
        metric!(counter("js.found_via_scraping") += self.found_via_scraping);
    }
}

/// Strips the hostname (or leading tilde) from the `path` and returns the path following the
/// hostname, with a leading `/`.
pub fn strip_hostname(path: &str) -> &str {
    if let Some(after_tilde) = path.strip_prefix('~') {
        return after_tilde;
    }

    if let Some((_scheme, rest)) = path.split_once("://") {
        return rest.find('/').map(|idx| &rest[idx..]).unwrap_or(rest);
    }
    path
}

/// Extracts a "file stem" from a path.
/// This is the `"/path/to/file"` in `"./path/to/file.min.js?foo=bar"`.
/// We use the most generic variant instead here, as server-side filtering is using a partial
/// match on the whole artifact path, thus `index.js` will be fetched no matter it's stored
/// as `~/index.js`, `~/index.js?foo=bar`, `http://example.com/index.js`,
/// or `http://example.com/index.js?foo=bar`.
// NOTE: We do want a leading slash to be included, eg. `/bundle/app.js` or `/index.js`,
// as it's not possible to use artifacts without proper host or `~/` wildcard.
fn extract_file_stem(path: &str) -> String {
    let path = strip_hostname(path);

    path.rsplit_once('/')
        .map(|(prefix, name)| {
            let name = name.split_once('.').map(|(stem, _)| stem).unwrap_or(name);
            format!("{prefix}/{name}")
        })
        .unwrap_or(path.to_owned())
}

/// Transforms a full absolute url into 2 or 4 generalized options.
// Based on `ReleaseFile.normalize`, see:
// https://github.com/getsentry/sentry/blob/master/src/sentry/models/releasefile.py
fn get_release_file_candidate_urls(url: &str) -> impl Iterator<Item = String> {
    let url = url.split('#').next().unwrap_or(url);
    let relative = strip_hostname(url);

    let urls = [
        // Absolute without fragment
        Some(url.to_string()),
        // Absolute without query
        url.split_once('?').map(|s| s.0.to_string()),
        // Relative without fragment
        Some(format!("~{relative}")),
        // Relative without query
        relative.split_once('?').map(|s| format!("~{}", s.0)),
    ];

    urls.into_iter().flatten()
}

/// Joins together frames `abs_path` and discovered sourcemap reference.
fn resolve_sourcemap_url(
    abs_path: &str,
    artifact_headers: &ArtifactHeaders,
    artifact_source: &str,
) -> Option<SourceMapUrl> {
    if let Some(header) = artifact_headers.get("sourcemap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else if let Some(header) = artifact_headers.get("x-sourcemap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else {
        let sm_ref = discover_sourcemaps_location(artifact_source)?;
        SourceMapUrl::parse_with_prefix(abs_path, sm_ref).ok()
    }
}

#[derive(Clone, Debug)]
pub struct FetchSourceMapCacheInternal {
    source: ByteViewString,
    sourcemap: ByteViewString,
}

impl CacheItemRequest for FetchSourceMapCacheInternal {
    type Item = OwnedSourceMapCache;

    const VERSIONS: CacheVersions = SOURCEMAP_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(async move {
            write_sourcemap_cache(temp_file.as_file_mut(), &self.source, &self.sourcemap)
        })
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        parse_sourcemap_cache_owned(data)
    }
}

fn parse_sourcemap_cache_owned(byteview: ByteView<'static>) -> CacheEntry<OwnedSourceMapCache> {
    SelfCell::try_new(byteview, |p| unsafe {
        SourceMapCache::parse(&*p).map_err(CacheError::from_std_error)
    })
}

/// Computes and writes the SourceMapCache.
#[tracing::instrument(skip_all)]
fn write_sourcemap_cache(file: &mut File, source: &str, sourcemap: &str) -> CacheEntry {
    tracing::debug!("Converting SourceMap cache");

    let smcache_writer = SourceMapCacheWriter::new(source, sourcemap)
        .map_err(|err| CacheError::Malformed(err.to_string()))?;

    let mut writer = BufWriter::new(file);
    smcache_writer.serialize(&mut writer)?;
    let file = writer.into_inner().map_err(io::Error::from)?;
    file.sync_all()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_hostname() {
        assert_eq!(strip_hostname("/absolute/unix/path"), "/absolute/unix/path");
        assert_eq!(strip_hostname("~/with/tilde"), "/with/tilde");
        assert_eq!(strip_hostname("https://example.com/"), "/");
        assert_eq!(
            strip_hostname("https://example.com/some/path/file.js"),
            "/some/path/file.js"
        );
    }

    #[test]
    fn test_get_release_file_candidate_urls() {
        let url = "https://example.com/assets/bundle.min.js";
        let expected = &[
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz";
        let expected = &[
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js#wat";
        let expected = &[
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat";
        let expected = &[
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);

        let url = "app:///_next/server/pages/_error.js";
        let expected = &[
            "app:///_next/server/pages/_error.js",
            "~/_next/server/pages/_error.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);
    }

    #[test]
    fn test_extract_file_stem() {
        let url = "https://example.com/bundle.js";
        assert_eq!(extract_file_stem(url), "/bundle");

        let url = "https://example.com/bundle.min.js";
        assert_eq!(extract_file_stem(url), "/bundle");

        let url = "https://example.com/assets/bundle.js";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js#wat";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        // app:// urls
        assert_eq!(
            extract_file_stem("app:///_next/server/pages/_error.js"),
            "/_next/server/pages/_error"
        );
        assert_eq!(
            extract_file_stem("app:///polyfills.e9f8f1606b76a9c9.js"),
            "/polyfills"
        );
    }

    #[test]
    fn joining_paths() {
        // (http) URLs
        let base = "https://example.com/path/to/assets/bundle.min.js?foo=1&bar=baz#wat";

        // relative
        assert_eq!(
            join_paths(base, "../sourcemaps/bundle.min.js.map"),
            "https://example.com/path/to/sourcemaps/bundle.min.js.map"
        );
        // absolute
        assert_eq!(join_paths(base, "/foo.js"), "https://example.com/foo.js");
        // absolute with tilde
        assert_eq!(join_paths(base, "~/foo.js"), "https://example.com/foo.js");

        // dots
        assert_eq!(
            join_paths(base, ".././.././to/./sourcemaps/./bundle.min.js.map"),
            "https://example.com/path/to/sourcemaps/bundle.min.js.map"
        );

        // file paths
        let base = "/home/foo/bar/baz.js";

        // relative
        assert_eq!(
            join_paths(base, "../sourcemaps/bundle.min.js.map"),
            "/home/foo/sourcemaps/bundle.min.js.map"
        );
        // absolute
        assert_eq!(join_paths(base, "/foo.js"), "/foo.js");
        // absolute with tilde
        assert_eq!(join_paths(base, "~/foo.js"), "/foo.js");

        // absolute path with its own scheme
        let path = "webpack:///../node_modules/scheduler/cjs/scheduler.production.min.js";
        assert_eq!(join_paths("http://example.com", path), path);

        // path with a dot in the middle
        assert_eq!(
            join_paths("http://example.com", "path/./to/file.min.js"),
            "http://example.com/path/to/file.min.js"
        );

        assert_eq!(
            join_paths("/playground/öut path/rollup/entrypoint1.js", "~/0.js.map"),
            "/0.js.map"
        );

        // path with a leading dot
        assert_eq!(
            join_paths(
                "app:///_next/static/chunks/pages/_app-569c402ef19f6d7b.js.map",
                "./node_modules/@sentry/browser/esm/integrations/trycatch.js"
            ),
            "app:///_next/static/chunks/pages/node_modules/@sentry/browser/esm/integrations/trycatch.js"
        );

        // webpack with only a single slash
        assert_eq!(
            join_paths(
                "app:///main-es2015.6216307eafb7335c4565.js.map",
                "webpack:/node_modules/@angular/core/__ivy_ngcc__/fesm2015/core.js"
            ),
            "webpack:/node_modules/@angular/core/__ivy_ngcc__/fesm2015/core.js"
        );

        // double-slash in the middle
        assert_eq!(
            join_paths(
                "https://foo.cloudfront.net/static//js/npm.sentry.d8b531aaf5202ddb7e90.js",
                "npm.sentry.d8b531aaf5202ddb7e90.js.map"
            ),
            "https://foo.cloudfront.net/static/js/npm.sentry.d8b531aaf5202ddb7e90.js.map"
        );

        // tests ported from python:
        // <https://github.com/getsentry/sentry/blob/ae9c0d8a33d509d9719a5a03e06c9797741877e9/tests/sentry/utils/test_urls.py#L22>
        assert_eq!(
            join_paths("http://example.com/foo", "bar"),
            "http://example.com/bar"
        );
        assert_eq!(
            join_paths("http://example.com/foo", "/bar"),
            "http://example.com/bar"
        );
        assert_eq!(
            join_paths("https://example.com/foo", "/bar"),
            "https://example.com/bar"
        );
        assert_eq!(
            join_paths("http://example.com/foo/baz", "bar"),
            "http://example.com/foo/bar"
        );
        assert_eq!(
            join_paths("http://example.com/foo/baz", "/bar"),
            "http://example.com/bar"
        );
        assert_eq!(
            join_paths("aps://example.com/foo", "/bar"),
            "aps://example.com/bar"
        );
        assert_eq!(
            join_paths("apsunknown://example.com/foo", "/bar"),
            "apsunknown://example.com/bar"
        );
        assert_eq!(
            join_paths("apsunknown://example.com/foo", "//aha/uhu"),
            "apsunknown://aha/uhu"
        );
    }

    #[test]
    fn data_urls() {
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data"),
            Ok(SourceMapUrl::Remote("/data".into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:"),
            Err(CacheError::Malformed("invalid `data:` url".into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:,foo"),
            Ok(SourceMapUrl::Data(String::from("foo").into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:,Hello%2C%20World%21"),
            Ok(SourceMapUrl::Data(String::from("Hello, World!").into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:;base64,SGVsbG8sIFdvcmxkIQ=="),
            Ok(SourceMapUrl::Data(String::from("Hello, World!").into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:;base64,SGVsbG8sIFdvcmxkIQ="),
            Err(CacheError::Malformed("invalid `data:` url".into())),
        );
    }
}
