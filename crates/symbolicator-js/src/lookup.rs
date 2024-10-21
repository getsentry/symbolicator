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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::{self, Write};
use std::sync::Arc;
use std::time::SystemTime;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symbolic::common::{ByteView, DebugId, SelfCell};
use symbolic::debuginfo::js::{discover_debug_id, discover_sourcemaps_location};
use symbolic::debuginfo::sourcebundle::{
    SourceBundleDebugSession, SourceFileDescriptor, SourceFileType,
};
use symbolic::debuginfo::Object;
use symbolic::sourcemapcache::SourceMapCache;
use symbolicator_sources::{
    HttpRemoteFile, ObjectType, RemoteFile, RemoteFileUri, SentryFileId, SentrySourceConfig,
};

use symbolicator_service::caches::{ByteViewString, SourceFilesCache};
use symbolicator_service::caching::{CacheEntry, CacheError, CacheKey, CacheKeyBuilder, Cacher};
use symbolicator_service::download::DownloadService;
use symbolicator_service::objects::{ObjectHandle, ObjectMetaHandle, ObjectsActor};
use symbolicator_service::types::{Scope, ScrapingConfig};
use symbolicator_service::utils::http::is_valid_origin;

use crate::api_lookup::{ArtifactHeaders, JsLookupResult, SentryLookupApi};
use crate::bundle_lookup::FileInBundleCache;
use crate::interface::{
    JsScrapingAttempt, JsScrapingFailureReason, JsStacktrace, ResolvedWith,
    SymbolicateJsStacktraces,
};
use crate::metrics::JsMetrics;
use crate::sourcemap_cache::{FetchSourceMapCacheInternal, SourceMapContents};
use crate::utils::{
    cache_busting_key, extract_file_stem, get_release_file_candidate_urls, join_paths,
    resolve_sourcemap_url,
};
use crate::SourceMapService;

pub type OwnedSourceMapCache = SelfCell<ByteView<'static>, SourceMapCache<'static>>;

// We want to cache scraped files for 1 hour, or rather, we want to re-download them every hour.
const SCRAPE_FILES_EVERY: u64 = 60 * 60;

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
    pub async fn new(service: SourceMapService, request: SymbolicateJsStacktraces) -> Self {
        let SourceMapService {
            objects,
            files_in_bundles,
            sourcefiles_cache,
            sourcemap_caches,
            download_svc,
            api_lookup,
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
            files_in_bundles,
            sourcefiles_cache,
            sourcemap_caches,
            api_lookup,
            download_svc,

            scope,
            source,

            release,
            dist,
            scraping,

            artifact_bundles: Default::default(),
            individual_artifacts: Default::default(),

            used_artifact_bundles: Default::default(),

            metrics: Default::default(),

            scraping_attempts: Default::default(),
        };

        Self {
            modules_by_abs_path,
            files_by_key: Default::default(),
            fetcher,
        }
    }

    /// Prepares the modules for processing
    // This lint is currently buggy
    #[allow(clippy::assigning_clones)]
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

    /// Consumes `self` and returns the artifact bundles that were used and
    /// the scraping attempts that were made.
    pub fn into_records(self) -> (HashSet<SentryFileId>, Vec<JsScrapingAttempt>) {
        (
            self.fetcher.used_artifact_bundles,
            self.fetcher.scraping_attempts,
        )
    }
}

/// A URL to a sourcemap file.
///
/// May either be a conventional URL or a data URL containing the sourcemap
/// encoded as BASE64.
#[derive(Clone, PartialEq)]
pub enum SourceMapUrl {
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
    pub fn parse_with_prefix(base: &str, url_string: &str) -> CacheEntry<Self> {
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

pub type ArtifactBundle = SelfCell<Arc<ObjectHandle>, SourceBundleDebugSession<'static>>;

/// The lookup key of an arbitrary file.
#[derive(Debug, Hash, PartialEq, Eq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKey {
    /// This key represents a [`SourceFileType::MinifiedSource`].
    MinifiedSource {
        abs_path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        debug_id: Option<DebugId>,
    },
    /// This key represents a [`SourceFileType::SourceMap`].
    SourceMap {
        #[serde(skip_serializing_if = "Option::is_none")]
        abs_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
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
    pub fn debug_id(&self) -> Option<DebugId> {
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
    pub fn as_type(&self) -> SourceFileType {
        match self {
            FileKey::MinifiedSource { .. } => SourceFileType::MinifiedSource,
            FileKey::SourceMap { .. } => SourceFileType::SourceMap,
            FileKey::Source { .. } => SourceFileType::Source,
        }
    }
}

/// The source of an individual file.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
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
    pub resolved_with: ResolvedWith,
}

impl<T> CachedFileEntry<T> {
    fn empty() -> Self {
        let uri = RemoteFileUri::new("<invalid>");
        Self {
            uri: CachedFileUri::IndividualFile(uri),
            entry: Err(CacheError::NotFound),
            resolved_with: ResolvedWith::Unknown,
        }
    }
}

/// This is very similar to `SourceFileDescriptor`, except that it is `'static` and includes just
/// the parts that we care about.
#[derive(Clone)]
pub struct CachedFile {
    pub contents: Option<ByteViewString>,
    sourcemap_url: Option<Arc<SourceMapUrl>>,
    debug_id: Option<DebugId>,
}

impl fmt::Debug for CachedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let contents: &str = self.contents();
        let contents = if contents.len() > 64 {
            // its just `Debug` prints, but we would like the end of the file, as it may
            // have a `sourceMappingURL`
            format!("...{}", &contents[contents.len() - 61..])
        } else {
            contents.to_string()
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
        let contents = Some(ByteViewString::from(contents));

        Ok(Self {
            contents,
            sourcemap_url: sourcemap_url.map(Arc::new),
            debug_id: None,
        })
    }

    pub fn contents(&self) -> &str {
        self.contents.as_deref().unwrap_or_default()
    }

    pub fn owned_contents(&self) -> ByteViewString {
        self.contents
            .clone()
            .unwrap_or(ByteViewString::from(String::new()))
    }

    /// Returns a string representation of a SourceMap URL if it was coming from a remote resource.
    pub fn sourcemap_url(&self) -> Option<String> {
        self.sourcemap_url
            .as_ref()
            .and_then(|sm| match sm.as_ref() {
                SourceMapUrl::Remote(url) => Some(url.clone()),
                SourceMapUrl::Data(_) => None,
            })
    }
}

#[derive(Debug)]
struct IndividualArtifact {
    remote_file: RemoteFile,
    headers: ArtifactHeaders,
    resolved_with: ResolvedWith,
}

pub type ArtifactBundles = BTreeMap<RemoteFileUri, CacheEntry<(ArtifactBundle, ResolvedWith)>>;

struct ArtifactFetcher {
    metrics: JsMetrics,

    // other services:
    /// Cache for looking up files in artifact bundles.
    ///
    /// This cache is shared between all JS symbolication requests.
    files_in_bundles: FileInBundleCache,
    objects: ObjectsActor,
    sourcefiles_cache: Arc<SourceFilesCache>,
    sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
    download_svc: Arc<DownloadService>,
    api_lookup: Arc<SentryLookupApi>,

    // source config
    scope: Scope,
    source: Arc<SentrySourceConfig>,

    // settings:
    release: Option<String>,
    dist: Option<String>,
    scraping: ScrapingConfig,

    /// The set of all the artifact bundles that we have downloaded so far.
    artifact_bundles: ArtifactBundles,
    /// The set of individual artifacts, by their `url`.
    individual_artifacts: HashMap<String, IndividualArtifact>,

    used_artifact_bundles: HashSet<SentryFileId>,

    scraping_attempts: Vec<JsScrapingAttempt>,
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
        if minified_source.entry.is_err() {
            self.metrics.record_not_found(SourceFileType::Source);
        }

        // Then fetch the corresponding sourcemap reference and debug_id
        let (sourcemap_url, source_debug_id) = match &minified_source.entry {
            Ok(minified_source) => (
                minified_source.sourcemap_url.as_deref(),
                minified_source.debug_id,
            ),
            Err(_) => (None, None),
        };

        let debug_id = debug_id.or(source_debug_id);

        // If we don't have sourcemap reference, nor a `DebugId`, we skip creating `SourceMapCache`.
        if sourcemap_url.is_none() && debug_id.is_none() {
            self.metrics.record_sourcemap_not_needed();
            return (minified_source, None);
        }

        // We have three cases here:
        let sourcemap = match sourcemap_url {
            // We have an embedded SourceMap via data URL
            Some(SourceMapUrl::Data(data)) => CachedFileEntry {
                uri: CachedFileUri::Embedded,
                entry: Ok(CachedFile {
                    contents: Some(data.clone()),
                    sourcemap_url: None,
                    debug_id,
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

        if matches!(sourcemap.uri, CachedFileUri::Embedded) {
            self.metrics.record_sourcemap_not_needed();
        } else if sourcemap.entry.is_err() {
            self.metrics.record_not_found(SourceFileType::SourceMap);
        }

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
        self.metrics.needed_files += 1;

        // Try looking up the file in one of the artifact bundles that we know about.
        let mut file = self.try_get_file_from_bundles(key);

        if file.is_none() {
            // Otherwise, try to get the file from an individual artifact.
            file = self.try_fetch_file_from_artifacts(key).await;
        }

        if file.is_none() {
            // Otherwise: Do a (cached) API lookup for the `abs_path` + `DebugId`
            if self.query_sentry_for_file(key).await {
                // At this point, *one* of our known artifacts includes the file we are looking for.
                // So we do the whole dance yet again.
                file = self.try_get_file_from_bundles(key);
                if file.is_none() {
                    file = self.try_fetch_file_from_artifacts(key).await;
                }
            }
        }

        if let Some(file) = file {
            if let Some(url) = key.abs_path() {
                self.scraping_attempts
                    .push(JsScrapingAttempt::not_attempted(url.to_owned()));
            }
            return file;
        }

        // Otherwise, fall back to scraping from the Web.
        self.scrape(key).await
    }

    /// Attempt to scrape a file from the web.
    async fn scrape(&mut self, key: &FileKey) -> CachedFileEntry {
        let Some(abs_path) = key.abs_path() else {
            return CachedFileEntry::empty();
        };

        let make_error = |err| CachedFileEntry {
            uri: CachedFileUri::ScrapedFile(RemoteFileUri::new(abs_path)),
            entry: Err(CacheError::DownloadError(err)),
            resolved_with: ResolvedWith::Unknown,
        };

        let mut url = match Url::parse(abs_path) {
            Ok(url) => url,
            Err(err) => {
                return make_error(err.to_string());
            }
        };

        if !self.scraping.enabled {
            self.scraping_attempts.push(JsScrapingAttempt::failure(
                abs_path.to_owned(),
                JsScrapingFailureReason::Disabled,
                String::new(),
            ));
            return make_error("Scraping disabled".to_string());
        }

        // Only scrape from http sources
        let scheme = url.scheme();
        if !["http", "https"].contains(&scheme) {
            self.scraping_attempts.push(JsScrapingAttempt::failure(
                abs_path.to_owned(),
                JsScrapingFailureReason::InvalidHost,
                format!("`{scheme}` is not an allowed download scheme"),
            ));
            return make_error(format!("`{scheme}` is not an allowed download scheme"));
        }

        if !is_valid_origin(&url, &self.scraping.allowed_origins) {
            self.scraping_attempts.push(JsScrapingAttempt::failure(
                abs_path.to_owned(),
                JsScrapingFailureReason::InvalidHost,
                format!("{abs_path} is not an allowed download origin"),
            ));
            return make_error(format!("{abs_path} is not an allowed download origin"));
        }

        match url.host_str() {
            None => {
                self.scraping_attempts.push(JsScrapingAttempt::failure(
                    abs_path.to_owned(),
                    JsScrapingFailureReason::InvalidHost,
                    String::new(),
                ));
                return make_error("Invalid host".to_string());
            }
            // NOTE: the reserved IPs cover a lot more than just localhost.
            Some(host @ ("localhost" | "127.0.0.1"))
                if !self.download_svc.can_connect_to_reserved_ips() =>
            {
                self.scraping_attempts.push(JsScrapingAttempt::failure(
                    abs_path.to_owned(),
                    JsScrapingFailureReason::InvalidHost,
                    format!("Can't connect to restricted host {host}"),
                ));
                return make_error("Invalid host".to_string());
            }
            _ => {}
        }

        self.metrics.scraped_files += 1;

        // We add a hash with a timestamp to the `url` to make sure that we are busting caches
        // that are based on the `uri`.
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cache_key = cache_busting_key(url.as_str(), timestamp, SCRAPE_FILES_EVERY);
        url.set_fragment(Some(&cache_key.to_string()));

        let mut remote_file = HttpRemoteFile::from_url(url, self.scraping.verify_ssl);
        remote_file.headers.extend(
            self.scraping
                .headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        let remote_file: RemoteFile = remote_file.into();
        let uri = CachedFileUri::ScrapedFile(remote_file.uri());
        // NOTE: We want to avoid using shared cache in this case, as fetching will be ineffective,
        // and storing would only store things we are re-fetching every couple of hours anyway.
        let scraped_file = self
            .sourcefiles_cache
            .fetch_file(&self.scope, remote_file, false)
            .await;

        let entry = match scraped_file {
            Ok(contents) => {
                self.metrics.record_file_scraped(key.as_type());
                tracing::trace!(?key, "Found file by scraping the web");

                let sourcemap_url = discover_sourcemaps_location(&contents)
                    .and_then(|sm_ref| SourceMapUrl::parse_with_prefix(abs_path, sm_ref).ok())
                    .map(Arc::new);

                let debug_id = discover_debug_id(&contents);

                self.scraping_attempts
                    .push(JsScrapingAttempt::success(abs_path.to_owned()));

                Ok(CachedFile {
                    contents: Some(contents),
                    sourcemap_url,
                    debug_id,
                })
            }
            Err(e) => {
                self.scraping_attempts.push(JsScrapingAttempt {
                    url: abs_path.to_owned(),
                    result: e.clone().into(),
                });

                Err(e)
            }
        };
        CachedFileEntry {
            uri,
            entry,
            resolved_with: ResolvedWith::Scraping,
        }
    }

    fn try_get_file_from_bundles(&mut self, key: &FileKey) -> Option<CachedFileEntry> {
        if self.artifact_bundles.is_empty() {
            return None;
        }

        // First see if we have already cached this file for any of this event's bundles.
        if let Some((bundle_uri, file_entry, resolved_with)) = self
            .files_in_bundles
            .try_get(self.artifact_bundles.keys().rev().cloned(), key.clone())
        {
            // we would like to gather metrics for which method we used to resolve the artifact bundle
            // containing the file. we should also be doing so if we got the file from the cache.
            if let Some(Ok((_, bundle_resolved_with))) = self.artifact_bundles.get(&bundle_uri) {
                self.metrics.record_file_found_in_bundle(
                    key.as_type(),
                    resolved_with,
                    *bundle_resolved_with,
                );
            }

            return Some(file_entry);
        }

        // If we have a `DebugId`, we try a lookup based on that.
        if let Some(debug_id) = key.debug_id() {
            let ty = key.as_type();
            for (bundle_uri, bundle) in self.artifact_bundles.iter().rev() {
                let Ok((bundle, bundle_resolved_with)) = bundle else {
                    continue;
                };
                let bundle = bundle.get();
                if let Ok(Some(descriptor)) = bundle.source_by_debug_id(debug_id, ty) {
                    self.metrics.record_file_found_in_bundle(
                        key.as_type(),
                        ResolvedWith::DebugId,
                        *bundle_resolved_with,
                    );
                    tracing::trace!(?key, "Found file in artifact bundles by debug-id");
                    let file_entry = CachedFileEntry {
                        uri: CachedFileUri::Bundled(bundle_uri.clone(), key.clone()),
                        entry: CachedFile::from_descriptor(key.abs_path(), descriptor),
                        resolved_with: ResolvedWith::DebugId,
                    };
                    self.files_in_bundles.insert(
                        bundle_uri,
                        key,
                        ResolvedWith::DebugId,
                        &file_entry,
                    );
                    return Some(file_entry);
                }
            }
        }

        // Otherwise, try all the candidate `abs_path` patterns in every artifact bundle.
        if let Some(abs_path) = key.abs_path() {
            for url in get_release_file_candidate_urls(abs_path) {
                for (bundle_uri, bundle) in self.artifact_bundles.iter().rev() {
                    let Ok((bundle, resolved_with)) = bundle else {
                        continue;
                    };
                    let bundle = bundle.get();
                    if let Ok(Some(descriptor)) = bundle.source_by_url(&url) {
                        self.metrics.record_file_found_in_bundle(
                            key.as_type(),
                            ResolvedWith::Url,
                            *resolved_with,
                        );
                        tracing::trace!(?key, url, "Found file in artifact bundles by url");
                        let file_entry = CachedFileEntry {
                            uri: CachedFileUri::Bundled(bundle_uri.clone(), key.clone()),
                            entry: CachedFile::from_descriptor(Some(abs_path), descriptor),
                            resolved_with: *resolved_with,
                        };
                        self.files_in_bundles.insert(
                            bundle_uri,
                            key,
                            ResolvedWith::Url,
                            &file_entry,
                        );
                        return Some(file_entry);
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
        self.metrics.fetched_artifacts += 1;

        let mut artifact_contents = self
            .sourcefiles_cache
            .fetch_file(&self.scope, artifact.remote_file.clone(), true)
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
                let debug_id = discover_debug_id(&contents);
                CachedFile {
                    contents: Some(contents),
                    sourcemap_url: sourcemap_url.map(Arc::new),
                    debug_id,
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
        self.metrics.api_requests += 1;

        let results = match self
            .api_lookup
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
                    self.metrics.queried_artifacts += 1;
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
                    self.metrics.queried_bundles += 1;

                    did_get_new_data |= self
                        .ensure_artifact_bundle(remote_file, resolved_with)
                        .await;
                }
            }
        }

        did_get_new_data
    }

    async fn ensure_artifact_bundle(
        &mut self,
        remote_file: RemoteFile,
        resolved_with: ResolvedWith,
    ) -> bool {
        let uri = remote_file.uri();
        // clippy, you are wrong, as this would result in borrowing errors,
        // because we are calling a `self` method while borrowing from `self`
        #[allow(clippy::map_entry)]
        if self.artifact_bundles.contains_key(&uri) {
            return false;
        }

        let artifact_bundle = self.fetch_artifact_bundle(remote_file).await;
        self.artifact_bundles
            .insert(uri, artifact_bundle.map(|bundle| (bundle, resolved_with)));

        true
    }

    #[tracing::instrument(skip(self))]
    async fn fetch_artifact_bundle(&self, file: RemoteFile) -> CacheEntry<ArtifactBundle> {
        let object_handle = ObjectMetaHandle::for_scoped_file(self.scope.clone(), file);

        let fetched_bundle = self.objects.fetch(object_handle).await?;
        open_bundle(fetched_bundle)
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
                    let source_content = source_entry.owned_contents();

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
                            sourcemap_entry.contents().as_bytes(),
                        );

                        cache_key.build()
                    };

                    let sourcemap = SourceMapContents::from_cachedfile(
                        &self.artifact_bundles,
                        &sourcemap.uri,
                        sourcemap_entry,
                    )
                    .unwrap_or_else(|| {
                        tracing::error!("expected either a `Bundled` or `Resolved` SourceMap");
                        SourceMapContents::Resolved(ByteViewString::from(String::new()))
                    });

                    let req = FetchSourceMapCacheInternal {
                        source: source_content,
                        sourcemap,
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
        let artifact_bundles = self.artifact_bundles.len() as u64;
        self.metrics.submit_metrics(artifact_bundles);
    }
}

/// This opens `artifact_bundle` [`Object`], ensuring that it is a [`SourceBundle`](`Object::SourceBundle`),
/// and opening up a debug session to it.
pub fn open_bundle(artifact_bundle: Arc<ObjectHandle>) -> CacheEntry<ArtifactBundle> {
    SelfCell::try_new(artifact_bundle, |handle| unsafe {
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
