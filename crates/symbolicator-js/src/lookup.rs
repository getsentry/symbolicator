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
use symbolic::debuginfo::Object;
use symbolic::debuginfo::js::{discover_debug_id, discover_sourcemaps_location};
use symbolic::debuginfo::sourcebundle::{
    SourceBundleDebugSession, SourceFileDescriptor, SourceFileType,
};
use symbolic::sourcemapcache::SourceMapCache;
use symbolicator_sources::{
    HttpRemoteFile, RemoteFile, RemoteFileUri, SentryFileId, SentrySourceConfig,
};

use symbolicator_service::caches::{ByteViewString, SourceFilesCache};
use symbolicator_service::caching::{CacheContents, CacheError, CacheKey, CacheKeyBuilder, Cacher};
use symbolicator_service::download::DownloadService;
use symbolicator_service::objects::{ObjectHandle, ObjectMetaHandle, ObjectsActor};
use symbolicator_service::types::{Scope, ScrapingConfig};
use symbolicator_service::utils::http::is_valid_origin;

use crate::SourceMapService;
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
            let debug_id = module.debug_id.parse().ok();
            if debug_id.is_none() {
                tracing::error!(debug_id = module.debug_id, "Invalid module debug ID");
            }
            let cached_module = SourceMapModule::new(&module.code_file, debug_id);

            modules_by_abs_path.insert(module.code_file.to_owned(), cached_module);
        }

        let metrics = JsMetrics::new(scope.as_ref().parse().ok());

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

            metrics,

            scraping_attempts: Default::default(),
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
    pub fn parse_with_prefix(base: &str, url_string: &str) -> CacheContents<Self> {
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
    pub entry: CacheContents<T>,
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
    ) -> CacheContents<Self> {
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

pub type ArtifactBundles = BTreeMap<RemoteFileUri, CacheContents<(ArtifactBundle, ResolvedWith)>>;

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
    #[tracing::instrument(skip(self))]
    async fn fetch_minified_and_sourcemap(
        &mut self,
        abs_path: String,
        debug_id: Option<DebugId>,
    ) -> (
        CachedFileEntry,
        Option<CachedFileEntry<OwnedSourceMapCache>>,
    ) {
        // First, check if we have already cached / created the `SourceMapCache`.
        let key = FileKey::MinifiedSource {
            abs_path: abs_path.clone(),
            debug_id,
        };

        // Fetch the minified file first
        let minified_source = self.get_file(&key).await;
        if minified_source.entry.is_err() {
            self.metrics
                .record_not_found(SourceFileType::Source, debug_id.is_some());

            // Temporarily sample cases of a file not being found even though it has a debug id.
            if let Some(debug_id) = debug_id {
                if rand::random::<f64>() < 0.0001 {
                    tracing::error!(
                        source_url = %self.source.url,
                        abs_path,
                        %debug_id,
                        "Failed to fetch source with debug id"
                    );
                }
            }
        }

        // Attach the minified file to the scope as a context
        sentry::configure_scope(|scope| {
            let mut minified_file_context = BTreeMap::new();
            if let Ok(value) = serde_json::to_value(&minified_source.uri) {
                minified_file_context.insert("URI".to_owned(), value);
            }
            if let Ok(value) = serde_json::to_value(minified_source.resolved_with) {
                minified_file_context.insert("Resolved with".to_owned(), value);
            }
            if let Ok(entry) = minified_source.entry.as_ref() {
                let sourcemap_url = match entry.sourcemap_url.as_deref() {
                    Some(SourceMapUrl::Data(_)) => serde_json::Value::String("Data".to_owned()),
                    Some(SourceMapUrl::Remote(url)) => serde_json::Value::String(url.clone()),
                    None => serde_json::Value::Null,
                };
                minified_file_context.insert("Sourcemap URL".to_owned(), sourcemap_url);

                if let Ok(value) = serde_json::to_value(entry.debug_id) {
                    minified_file_context.insert("Debug ID".to_owned(), value);
                }
            }

            scope.set_context(
                "Minified file",
                sentry::protocol::Context::Other(minified_file_context),
            );
        });

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
            self.metrics
                .record_not_found(SourceFileType::SourceMap, debug_id.is_some());

            // Temporarily sample cases of a file not being found even though it has a debug id.
            if let Some(debug_id) = debug_id {
                if rand::random::<f64>() < 0.0001 {
                    tracing::error!(
                        source_url = %self.source.url,
                        abs_path,
                        %debug_id,
                        "Failed to fetch sourcemap with debug id"
                    );
                }
            }
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
    #[tracing::instrument(skip(self))]
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

        let entry = match scraped_file.into_contents() {
            Ok(contents) => {
                self.metrics
                    .record_file_scraped(key.as_type(), key.debug_id().is_some());
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

    #[tracing::instrument(skip(self))]
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
                    key.debug_id().is_some(),
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
                        key.debug_id().is_some(),
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
                            key.debug_id().is_some(),
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

    #[tracing::instrument(skip(self))]
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
            .await
            .into_contents();

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
    #[tracing::instrument(skip(self))]
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
    #[tracing::instrument(skip_all)]
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
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    lookup_url = %self.source.url,
                    "Failed to query Sentry for files",
                );
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
        if self.artifact_bundles.contains_key(&uri) {
            return false;
        }

        let artifact_bundle = self.fetch_artifact_bundle(remote_file).await;
        self.artifact_bundles
            .insert(uri, artifact_bundle.map(|bundle| (bundle, resolved_with)));

        true
    }

    #[tracing::instrument(skip(self))]
    async fn fetch_artifact_bundle(&self, file: RemoteFile) -> CacheContents<ArtifactBundle> {
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
                    self.sourcemap_caches
                        .compute_memoized(req, cache_key)
                        .await
                        .into_contents()
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
pub fn open_bundle(artifact_bundle: Arc<ObjectHandle>) -> CacheContents<ArtifactBundle> {
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

#[cfg(test)]
mod tests {
    use super::SourceMapUrl;

    #[test]
    fn test_data_url() {
        let url = "data:application/javascript;base64,dmFyIF9fY3JlYXRlID0gT2JqZWN0LmNyZWF0ZTsKdmFyIF9fZGVmUHJvcCA9IE9iamVjdC5kZWZpbmVQcm9wZXJ0eTsKdmFyIF9fZ2V0T3duUHJvcERlc2MgPSBPYmplY3QuZ2V0T3duUHJvcGVydHlEZXNjcmlwdG9yOwp2YXIgX19nZXRPd25Qcm9wTmFtZXMgPSBPYmplY3QuZ2V0T3duUHJvcGVydHlOYW1lczsKdmFyIF9fZ2V0UHJvdG9PZiA9IE9iamVjdC5nZXRQcm90b3R5cGVPZjsKdmFyIF9faGFzT3duUHJvcCA9IE9iamVjdC5wcm90b3R5cGUuaGFzT3duUHJvcGVydHk7CnZhciBfX2NvbW1vbkpTID0gKGNiLCBtb2QpID0+IGZ1bmN0aW9uIF9fcmVxdWlyZSgpIHsKICByZXR1cm4gbW9kIHx8ICgwLCBjYltfX2dldE93blByb3BOYW1lcyhjYilbMF1dKSgobW9kID0geyBleHBvcnRzOiB7fSB9KS5leHBvcnRzLCBtb2QpLCBtb2QuZXhwb3J0czsKfTsKdmFyIF9fY29weVByb3BzID0gKHRvLCBmcm9tLCBleGNlcHQsIGRlc2MpID0+IHsKICBpZiAoZnJvbSAmJiB0eXBlb2YgZnJvbSA9PT0gIm9iamVjdCIgfHwgdHlwZW9mIGZyb20gPT09ICJmdW5jdGlvbiIpIHsKICAgIGZvciAobGV0IGtleSBvZiBfX2dldE93blByb3BOYW1lcyhmcm9tKSkKICAgICAgaWYgKCFfX2hhc093blByb3AuY2FsbCh0bywga2V5KSAmJiBrZXkgIT09IGV4Y2VwdCkKICAgICAgICBfX2RlZlByb3AodG8sIGtleSwgeyBnZXQ6ICgpID0+IGZyb21ba2V5XSwgZW51bWVyYWJsZTogIShkZXNjID0gX19nZXRPd25Qcm9wRGVzYyhmcm9tLCBrZXkpKSB8fCBkZXNjLmVudW1lcmFibGUgfSk7CiAgfQogIHJldHVybiB0bzsKfTsKdmFyIF9fdG9FU00gPSAobW9kLCBpc05vZGVNb2RlLCB0YXJnZXQpID0+ICh0YXJnZXQgPSBtb2QgIT0gbnVsbCA/IF9fY3JlYXRlKF9fZ2V0UHJvdG9PZihtb2QpKSA6IHt9LCBfX2NvcHlQcm9wcygKICAvLyBJZiB0aGUgaW1wb3J0ZXIgaXMgaW4gbm9kZSBjb21wYXRpYmlsaXR5IG1vZGUgb3IgdGhpcyBpcyBub3QgYW4gRVNNCiAgLy8gZmlsZSB0aGF0IGhhcyBiZWVuIGNvbnZlcnRlZCB0byBhIENvbW1vbkpTIGZpbGUgdXNpbmcgYSBCYWJlbC0KICAvLyBjb21wYXRpYmxlIHRyYW5zZm9ybSAoaS5lLiAiX19lc01vZHVsZSIgaGFzIG5vdCBiZWVuIHNldCksIHRoZW4gc2V0CiAgLy8gImRlZmF1bHQiIHRvIHRoZSBDb21tb25KUyAibW9kdWxlLmV4cG9ydHMiIGZvciBub2RlIGNvbXBhdGliaWxpdHkuCiAgaXNOb2RlTW9kZSB8fCAhbW9kIHx8ICFtb2QuX19lc01vZHVsZSA/IF9fZGVmUHJvcCh0YXJnZXQsICJkZWZhdWx0IiwgeyB2YWx1ZTogbW9kLCBlbnVtZXJhYmxlOiB0cnVlIH0pIDogdGFyZ2V0LAogIG1vZAopKTsKCi8vIChkaXNhYmxlZCk6Y3J5cHRvCnZhciByZXF1aXJlX2NyeXB0byA9IF9fY29tbW9uSlMoewogICIoZGlzYWJsZWQpOmNyeXB0byIoKSB7CiAgfQp9KTsKCi8vIChkaXNhYmxlZCk6YnVmZmVyCnZhciByZXF1aXJlX2J1ZmZlciA9IF9fY29tbW9uSlMoewogICIoZGlzYWJsZWQpOmJ1ZmZlciIoKSB7CiAgfQp9KTsKCi8vIC4uLy4uL25vZGVfbW9kdWxlcy9qcy1zaGEyNTYvc3JjL3NoYTI1Ni5qcwp2YXIgcmVxdWlyZV9zaGEyNTYgPSBfX2NvbW1vbkpTKHsKICAiLi4vLi4vbm9kZV9tb2R1bGVzL2pzLXNoYTI1Ni9zcmMvc2hhMjU2LmpzIihleHBvcnRzLCBtb2R1bGUpIHsKICAgIChmdW5jdGlvbigpIHsKICAgICAgInVzZSBzdHJpY3QiOwogICAgICB2YXIgRVJST1IgPSAiaW5wdXQgaXMgaW52YWxpZCB0eXBlIjsKICAgICAgdmFyIFdJTkRPVyA9IHR5cGVvZiB3aW5kb3cgPT09ICJvYmplY3QiOwogICAgICB2YXIgcm9vdCA9IFdJTkRPVyA/IHdpbmRvdyA6IHt9OwogICAgICBpZiAocm9vdC5KU19TSEEyNTZfTk9fV0lORE9XKSB7CiAgICAgICAgV0lORE9XID0gZmFsc2U7CiAgICAgIH0KICAgICAgdmFyIFdFQl9XT1JLRVIgPSAhV0lORE9XICYmIHR5cGVvZiBzZWxmID09PSAib2JqZWN0IjsKICAgICAgdmFyIE5PREVfSlMgPSAhcm9vdC5KU19TSEEyNTZfTk9fTk9ERV9KUyAmJiB0eXBlb2YgcHJvY2VzcyA9PT0gIm9iamVjdCIgJiYgcHJvY2Vzcy52ZXJzaW9ucyAmJiBwcm9jZXNzLnZlcnNpb25zLm5vZGUgJiYgcHJvY2Vzcy50eXBlICE9ICJyZW5kZXJlciI7CiAgICAgIGlmIChOT0RFX0pTKSB7CiAgICAgICAgcm9vdCA9IGdsb2JhbDsKICAgICAgfSBlbHNlIGlmIChXRUJfV09SS0VSKSB7CiAgICAgICAgcm9vdCA9IHNlbGY7CiAgICAgIH0KICAgICAgdmFyIENPTU1PTl9KUyA9ICFyb290LkpTX1NIQTI1Nl9OT19DT01NT05fSlMgJiYgdHlwZW9mIG1vZHVsZSA9PT0gIm9iamVjdCIgJiYgbW9kdWxlLmV4cG9ydHM7CiAgICAgIHZhciBBTUQgPSB0eXBlb2YgZGVmaW5lID09PSAiZnVuY3Rpb24iICYmIGRlZmluZS5hbWQ7CiAgICAgIHZhciBBUlJBWV9CVUZGRVIgPSAhcm9vdC5KU19TSEEyNTZfTk9fQVJSQVlfQlVGRkVSICYmIHR5cGVvZiBBcnJheUJ1ZmZlciAhPT0gInVuZGVmaW5lZCI7CiAgICAgIHZhciBIRVhfQ0hBUlMgPSAiMDEyMzQ1Njc4OWFiY2RlZiIuc3BsaXQoIiIpOwogICAgICB2YXIgRVhUUkEgPSBbLTIxNDc0ODM2NDgsIDgzODg2MDgsIDMyNzY4LCAxMjhdOwogICAgICB2YXIgU0hJRlQgPSBbMjQsIDE2LCA4LCAwXTsKICAgICAgdmFyIEsgPSBbCiAgICAgICAgMTExNjM1MjQwOCwKICAgICAgICAxODk5NDQ3NDQxLAogICAgICAgIDMwNDkzMjM0NzEsCiAgICAgICAgMzkyMTAwOTU3MywKICAgICAgICA5NjE5ODcxNjMsCiAgICAgICAgMTUwODk3MDk5MywKICAgICAgICAyNDUzNjM1NzQ4LAogICAgICAgIDI4NzA3NjMyMjEsCiAgICAgICAgMzYyNDM4MTA4MCwKICAgICAgICAzMTA1OTg0MDEsCiAgICAgICAgNjA3MjI1Mjc4LAogICAgICAgIDE0MjY4ODE5ODcsCiAgICAgICAgMTkyNTA3ODM4OCwKICAgICAgICAyMTYyMDc4MjA2LAogICAgICAgIDI2MTQ4ODgxMDMsCiAgICAgICAgMzI0ODIyMjU4MCwKICAgICAgICAzODM1MzkwNDAxLAogICAgICAgIDQwMjIyMjQ3NzQsCiAgICAgICAgMjY0MzQ3MDc4LAogICAgICAgIDYwNDgwNzYyOCwKICAgICAgICA3NzAyNTU5ODMsCiAgICAgICAgMTI0OTE1MDEyMiwKICAgICAgICAxNTU1MDgxNjkyLAogICAgICAgIDE5OTYwNjQ5ODYsCiAgICAgICAgMjU1NDIyMDg4MiwKICAgICAgICAyODIxODM0MzQ5LAogICAgICAgIDI5NTI5OTY4MDgsCiAgICAgICAgMzIxMDMxMzY3MSwKICAgICAgICAzMzM2NTcxODkxLAogICAgICAgIDM1ODQ1Mjg3MTEsCiAgICAgICAgMTEzOTI2OTkzLAogICAgICAgIDMzODI0MTg5NSwKICAgICAgICA2NjYzMDcyMDUsCiAgICAgICAgNzczNTI5OTEyLAogICAgICAgIDEyOTQ3NTczNzIsCiAgICAgICAgMTM5NjE4MjI5MSwKICAgICAgICAxNjk1MTgzNzAwLAogICAgICAgIDE5ODY2NjEwNTEsCiAgICAgICAgMjE3NzAyNjM1MCwKICAgICAgICAyNDU2OTU2MDM3LAogICAgICAgIDI3MzA0ODU5MjEsCiAgICAgICAgMjgyMDMwMjQxMSwKICAgICAgICAzMjU5NzMwODAwLAogICAgICAgIDMzNDU3NjQ3NzEsCiAgICAgICAgMzUxNjA2NTgxNywKICAgICAgICAzNjAwMzUyODA0LAogICAgICAgIDQwOTQ1NzE5MDksCiAgICAgICAgMjc1NDIzMzQ0LAogICAgICAgIDQzMDIyNzczNCwKICAgICAgICA1MDY5NDg2MTYsCiAgICAgICAgNjU5MDYwNTU2LAogICAgICAgIDg4Mzk5Nzg3NywKICAgICAgICA5NTgxMzk1NzEsCiAgICAgICAgMTMyMjgyMjIxOCwKICAgICAgICAxNTM3MDAyMDYzLAogICAgICAgIDE3NDc4NzM3NzksCiAgICAgICAgMTk1NTU2MjIyMiwKICAgICAgICAyMDI0MTA0ODE1LAogICAgICAgIDIyMjc3MzA0NTIsCiAgICAgICAgMjM2MTg1MjQyNCwKICAgICAgICAyNDI4NDM2NDc0LAogICAgICAgIDI3NTY3MzQxODcsCiAgICAgICAgMzIwNDAzMTQ3OSwKICAgICAgICAzMzI5MzI1Mjk4CiAgICAgIF07CiAgICAgIHZhciBPVVRQVVRfVFlQRVMgPSBbImhleCIsICJhcnJheSIsICJkaWdlc3QiLCAiYXJyYXlCdWZmZXIiXTsKICAgICAgdmFyIGJsb2NrcyA9IFtdOwogICAgICBpZiAocm9vdC5KU19TSEEyNTZfTk9fTk9ERV9KUyB8fCAhQXJyYXkuaXNBcnJheSkgewogICAgICAgIEFycmF5LmlzQXJyYXkgPSBmdW5jdGlvbihvYmopIHsKICAgICAgICAgIHJldHVybiBPYmplY3QucHJvdG90eXBlLnRvU3RyaW5nLmNhbGwob2JqKSA9PT0gIltvYmplY3QgQXJyYXldIjsKICAgICAgICB9OwogICAgICB9CiAgICAgIGlmIChBUlJBWV9CVUZGRVIgJiYgKHJvb3QuSlNfU0hBMjU2X05PX0FSUkFZX0JVRkZFUl9JU19WSUVXIHx8ICFBcnJheUJ1ZmZlci5pc1ZpZXcpKSB7CiAgICAgICAgQXJyYXlCdWZmZXIuaXNWaWV3ID0gZnVuY3Rpb24ob2JqKSB7CiAgICAgICAgICByZXR1cm4gdHlwZW9mIG9iaiA9PT0gIm9iamVjdCIgJiYgb2JqLmJ1ZmZlciAmJiBvYmouYnVmZmVyLmNvbnN0cnVjdG9yID09PSBBcnJheUJ1ZmZlcjsKICAgICAgICB9OwogICAgICB9CiAgICAgIHZhciBjcmVhdGVPdXRwdXRNZXRob2QgPSBmdW5jdGlvbihvdXRwdXRUeXBlLCBpczIyNCkgewogICAgICAgIHJldHVybiBmdW5jdGlvbihtZXNzYWdlKSB7CiAgICAgICAgICByZXR1cm4gbmV3IFNoYTI1NihpczIyNCwgdHJ1ZSkudXBkYXRlKG1lc3NhZ2UpW291dHB1dFR5cGVdKCk7CiAgICAgICAgfTsKICAgICAgfTsKICAgICAgdmFyIGNyZWF0ZU1ldGhvZCA9IGZ1bmN0aW9uKGlzMjI0KSB7CiAgICAgICAgdmFyIG1ldGhvZCA9IGNyZWF0ZU91dHB1dE1ldGhvZCgiaGV4IiwgaXMyMjQpOwogICAgICAgIGlmIChOT0RFX0pTKSB7CiAgICAgICAgICBtZXRob2QgPSBub2RlV3JhcChtZXRob2QsIGlzMjI0KTsKICAgICAgICB9CiAgICAgICAgbWV0aG9kLmNyZWF0ZSA9IGZ1bmN0aW9uKCkgewogICAgICAgICAgcmV0dXJuIG5ldyBTaGEyNTYoaXMyMjQpOwogICAgICAgIH07CiAgICAgICAgbWV0aG9kLnVwZGF0ZSA9IGZ1bmN0aW9uKG1lc3NhZ2UpIHsKICAgICAgICAgIHJldHVybiBtZXRob2QuY3JlYXRlKCkudXBkYXRlKG1lc3NhZ2UpOwogICAgICAgIH07CiAgICAgICAgZm9yICh2YXIgaSA9IDA7IGkgPCBPVVRQVVRfVFlQRVMubGVuZ3RoOyArK2kpIHsKICAgICAgICAgIHZhciB0eXBlID0gT1VUUFVUX1RZUEVTW2ldOwogICAgICAgICAgbWV0aG9kW3R5cGVdID0gY3JlYXRlT3V0cHV0TWV0aG9kKHR5cGUsIGlzMjI0KTsKICAgICAgICB9CiAgICAgICAgcmV0dXJuIG1ldGhvZDsKICAgICAgfTsKICAgICAgdmFyIG5vZGVXcmFwID0gZnVuY3Rpb24obWV0aG9kLCBpczIyNCkgewogICAgICAgIHZhciBjcnlwdG8gPSByZXF1aXJlX2NyeXB0bygpOwogICAgICAgIHZhciBCdWZmZXIyID0gcmVxdWlyZV9idWZmZXIoKS5CdWZmZXI7CiAgICAgICAgdmFyIGFsZ29yaXRobSA9IGlzMjI0ID8gInNoYTIyNCIgOiAic2hhMjU2IjsKICAgICAgICB2YXIgYnVmZmVyRnJvbTsKICAgICAgICBpZiAoQnVmZmVyMi5mcm9tICYmICFyb290LkpTX1NIQTI1Nl9OT19CVUZGRVJfRlJPTSkgewogICAgICAgICAgYnVmZmVyRnJvbSA9IEJ1ZmZlcjIuZnJvbTsKICAgICAgICB9IGVsc2UgewogICAgICAgICAgYnVmZmVyRnJvbSA9IGZ1bmN0aW9uKG1lc3NhZ2UpIHsKICAgICAgICAgICAgcmV0dXJuIG5ldyBCdWZmZXIyKG1lc3NhZ2UpOwogICAgICAgICAgfTsKICAgICAgICB9CiAgICAgICAgdmFyIG5vZGVNZXRob2QgPSBmdW5jdGlvbihtZXNzYWdlKSB7CiAgICAgICAgICBpZiAodHlwZW9mIG1lc3NhZ2UgPT09ICJzdHJpbmciKSB7CiAgICAgICAgICAgIHJldHVybiBjcnlwdG8uY3JlYXRlSGFzaChhbGdvcml0aG0pLnVwZGF0ZShtZXNzYWdlLCAidXRmOCIpLmRpZ2VzdCgiaGV4Iik7CiAgICAgICAgICB9IGVsc2UgewogICAgICAgICAgICBpZiAobWVzc2FnZSA9PT0gbnVsbCB8fCBtZXNzYWdlID09PSB2b2lkIDApIHsKICAgICAgICAgICAgICB0aHJvdyBuZXcgRXJyb3IoRVJST1IpOwogICAgICAgICAgICB9IGVsc2UgaWYgKG1lc3NhZ2UuY29uc3RydWN0b3IgPT09IEFycmF5QnVmZmVyKSB7CiAgICAgICAgICAgICAgbWVzc2FnZSA9IG5ldyBVaW50OEFycmF5KG1lc3NhZ2UpOwogICAgICAgICAgICB9CiAgICAgICAgICB9CiAgICAgICAgICBpZiAoQXJyYXkuaXNBcnJheShtZXNzYWdlKSB8fCBBcnJheUJ1ZmZlci5pc1ZpZXcobWVzc2FnZSkgfHwgbWVzc2FnZS5jb25zdHJ1Y3RvciA9PT0gQnVmZmVyMikgewogICAgICAgICAgICByZXR1cm4gY3J5cHRvLmNyZWF0ZUhhc2goYWxnb3JpdGhtKS51cGRhdGUoYnVmZmVyRnJvbShtZXNzYWdlKSkuZGlnZXN0KCJoZXgiKTsKICAgICAgICAgIH0gZWxzZSB7CiAgICAgICAgICAgIHJldHVybiBtZXRob2QobWVzc2FnZSk7CiAgICAgICAgICB9CiAgICAgICAgfTsKICAgICAgICByZXR1cm4gbm9kZU1ldGhvZDsKICAgICAgfTsKICAgICAgdmFyIGNyZWF0ZUhtYWNPdXRwdXRNZXRob2QgPSBmdW5jdGlvbihvdXRwdXRUeXBlLCBpczIyNCkgewogICAgICAgIHJldHVybiBmdW5jdGlvbihrZXksIG1lc3NhZ2UpIHsKICAgICAgICAgIHJldHVybiBuZXcgSG1hY1NoYTI1NihrZXksIGlzMjI0LCB0cnVlKS51cGRhdGUobWVzc2FnZSlbb3V0cHV0VHlwZV0oKTsKICAgICAgICB9OwogICAgICB9OwogICAgICB2YXIgY3JlYXRlSG1hY01ldGhvZCA9IGZ1bmN0aW9uKGlzMjI0KSB7CiAgICAgICAgdmFyIG1ldGhvZCA9IGNyZWF0ZUhtYWNPdXRwdXRNZXRob2QoImhleCIsIGlzMjI0KTsKICAgICAgICBtZXRob2QuY3JlYXRlID0gZnVuY3Rpb24oa2V5KSB7CiAgICAgICAgICByZXR1cm4gbmV3IEhtYWNTaGEyNTYoa2V5LCBpczIyNCk7CiAgICAgICAgfTsKICAgICAgICBtZXRob2QudXBkYXRlID0gZnVuY3Rpb24oa2V5LCBtZXNzYWdlKSB7CiAgICAgICAgICByZXR1cm4gbWV0aG9kLmNyZWF0ZShrZXkpLnVwZGF0ZShtZXNzYWdlKTsKICAgICAgICB9OwogICAgICAgIGZvciAodmFyIGkgPSAwOyBpIDwgT1VUUFVUX1RZUEVTLmxlbmd0aDsgKytpKSB7CiAgICAgICAgICB2YXIgdHlwZSA9IE9VVFBVVF9UWVBFU1tpXTsKICAgICAgICAgIG1ldGhvZFt0eXBlXSA9IGNyZWF0ZUhtYWNPdXRwdXRNZXRob2QodHlwZSwgaXMyMjQpOwogICAgICAgIH0KICAgICAgICByZXR1cm4gbWV0aG9kOwogICAgICB9OwogICAgICBmdW5jdGlvbiBTaGEyNTYoaXMyMjQsIHNoYXJlZE1lbW9yeSkgewogICAgICAgIGlmIChzaGFyZWRNZW1vcnkpIHsKICAgICAgICAgIGJsb2Nrc1swXSA9IGJsb2Nrc1sxNl0gPSBibG9ja3NbMV0gPSBibG9ja3NbMl0gPSBibG9ja3NbM10gPSBibG9ja3NbNF0gPSBibG9ja3NbNV0gPSBibG9ja3NbNl0gPSBibG9ja3NbN10gPSBibG9ja3NbOF0gPSBibG9ja3NbOV0gPSBibG9ja3NbMTBdID0gYmxvY2tzWzExXSA9IGJsb2Nrc1sxMl0gPSBibG9ja3NbMTNdID0gYmxvY2tzWzE0XSA9IGJsb2Nrc1sxNV0gPSAwOwogICAgICAgICAgdGhpcy5ibG9ja3MgPSBibG9ja3M7CiAgICAgICAgfSBlbHNlIHsKICAgICAgICAgIHRoaXMuYmxvY2tzID0gWzAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDAsIDBdOwogICAgICAgIH0KICAgICAgICBpZiAoaXMyMjQpIHsKICAgICAgICAgIHRoaXMuaDAgPSAzMjM4MzcxMDMyOwogICAgICAgICAgdGhpcy5oMSA9IDkxNDE1MDY2MzsKICAgICAgICAgIHRoaXMuaDIgPSA4MTI3MDI5OTk7CiAgICAgICAgICB0aGlzLmgzID0gNDE0NDkxMjY5NzsKICAgICAgICAgIHRoaXMuaDQgPSA0MjkwNzc1ODU3OwogICAgICAgICAgdGhpcy5oNSA9IDE3NTA2MDMwMjU7CiAgICAgICAgICB0aGlzLmg2ID0gMTY5NDA3NjgzOTsKICAgICAgICAgIHRoaXMuaDcgPSAzMjA0MDc1NDI4OwogICAgICAgIH0gZWxzZSB7CiAgICAgICAgICB0aGlzLmgwID0gMTc3OTAzMzcwMzsKICAgICAgICAgIHRoaXMuaDEgPSAzMTQ0MTM0Mjc3OwogICAgICAgICAgdGhpcy5oMiA9IDEwMTM5MDQyNDI7CiAgICAgICAgICB0aGlzLmgzID0gMjc3MzQ4MDc2MjsKICAgICAgICAgIHRoaXMuaDQgPSAxMzU5ODkzMTE5OwogICAgICAgICAgdGhpcy5oNSA9IDI2MDA4MjI5MjQ7CiAgICAgICAgICB0aGlzLmg2ID0gNTI4NzM0NjM1OwogICAgICAgICAgdGhpcy5oNyA9IDE1NDE0NTkyMjU7CiAgICAgICAgfQogICAgICAgIHRoaXMuYmxvY2sgPSB0aGlzLnN0YXJ0ID0gdGhpcy5ieXRlcyA9IHRoaXMuaEJ5dGVzID0gMDsKICAgICAgICB0aGlzLmZpbmFsaXplZCA9IHRoaXMuaGFzaGVkID0gZmFsc2U7CiAgICAgICAgdGhpcy5maXJzdCA9IHRydWU7CiAgICAgICAgdGhpcy5pczIyNCA9IGlzMjI0OwogICAgICB9CiAgICAgIFNoYTI1Ni5wcm90b3R5cGUudXBkYXRlID0gZnVuY3Rpb24obWVzc2FnZSkgewogICAgICAgIGlmICh0aGlzLmZpbmFsaXplZCkgewogICAgICAgICAgcmV0dXJuOwogICAgICAgIH0KICAgICAgICB2YXIgbm90U3RyaW5nLCB0eXBlID0gdHlwZW9mIG1lc3NhZ2U7CiAgICAgICAgaWYgKHR5cGUgIT09ICJzdHJpbmciKSB7CiAgICAgICAgICBpZiAodHlwZSA9PT0gIm9iamVjdCIpIHsKICAgICAgICAgICAgaWYgKG1lc3NhZ2UgPT09IG51bGwpIHsKICAgICAgICAgICAgICB0aHJvdyBuZXcgRXJyb3IoRVJST1IpOwogICAgICAgICAgICB9IGVsc2UgaWYgKEFSUkFZX0JVRkZFUiAmJiBtZXNzYWdlLmNvbnN0cnVjdG9yID09PSBBcnJheUJ1ZmZlcikgewogICAgICAgICAgICAgIG1lc3NhZ2UgPSBuZXcgVWludDhBcnJheShtZXNzYWdlKTsKICAgICAgICAgICAgfSBlbHNlIGlmICghQXJyYXkuaXNBcnJheShtZXNzYWdlKSkgewogICAgICAgICAgICAgIGlmICghQVJSQVlfQlVGRkVSIHx8ICFBcnJheUJ1ZmZlci5pc1ZpZXcobWVzc2FnZSkpIHsKICAgICAgICAgICAgICAgIHRocm93IG5ldyBFcnJvcihFUlJPUik7CiAgICAgICAgICAgICAgfQogICAgICAgICAgICB9CiAgICAgICAgICB9IGVsc2UgewogICAgICAgICAgICB0aHJvdyBuZXcgRXJyb3IoRVJST1IpOwogICAgICAgICAgfQogICAgICAgICAgbm90U3RyaW5nID0gdHJ1ZTsKICAgICAgICB9CiAgICAgICAgdmFyIGNvZGUsIGluZGV4ID0gMCwgaSwgbGVuZ3RoID0gbWVzc2FnZS5sZW5ndGgsIGJsb2NrczIgPSB0aGlzLmJsb2NrczsKICAgICAgICB3aGlsZSAoaW5kZXggPCBsZW5ndGgpIHsKICAgICAgICAgIGlmICh0aGlzLmhhc2hlZCkgewogICAgICAgICAgICB0aGlzLmhhc2hlZCA9IGZhbHNlOwogICAgICAgICAgICBibG9ja3MyWzBdID0gdGhpcy5ibG9jazsKICAgICAgICAgICAgdGhpcy5ibG9jayA9IGJsb2NrczJbMTZdID0gYmxvY2tzMlsxXSA9IGJsb2NrczJbMl0gPSBibG9ja3MyWzNdID0gYmxvY2tzMls0XSA9IGJsb2NrczJbNV0gPSBibG9ja3MyWzZdID0gYmxvY2tzMls3XSA9IGJsb2NrczJbOF0gPSBibG9ja3MyWzldID0gYmxvY2tzMlsxMF0gPSBibG9ja3MyWzExXSA9IGJsb2NrczJbMTJdID0gYmxvY2tzMlsxM10gPSBibG9ja3MyWzE0XSA9IGJsb2NrczJbMTVdID0gMDsKICAgICAgICAgIH0KICAgICAgICAgIGlmIChub3RTdHJpbmcpIHsKICAgICAgICAgICAgZm9yIChpID0gdGhpcy5zdGFydDsgaW5kZXggPCBsZW5ndGggJiYgaSA8IDY0OyArK2luZGV4KSB7CiAgICAgICAgICAgICAgYmxvY2tzMltpID4+PiAyXSB8PSBtZXNzYWdlW2luZGV4XSA8PCBTSElGVFtpKysgJiAzXTsKICAgICAgICAgICAgfQogICAgICAgICAgfSBlbHNlIHsKICAgICAgICAgICAgZm9yIChpID0gdGhpcy5zdGFydDsgaW5kZXggPCBsZW5ndGggJiYgaSA8IDY0OyArK2luZGV4KSB7CiAgICAgICAgICAgICAgY29kZSA9IG1lc3NhZ2UuY2hhckNvZGVBdChpbmRleCk7CiAgICAgICAgICAgICAgaWYgKGNvZGUgPCAxMjgpIHsKICAgICAgICAgICAgICAgIGJsb2NrczJbaSA+Pj4gMl0gfD0gY29kZSA8PCBTSElGVFtpKysgJiAzXTsKICAgICAgICAgICAgICB9IGVsc2UgaWYgKGNvZGUgPCAyMDQ4KSB7CiAgICAgICAgICAgICAgICBibG9ja3MyW2kgPj4+IDJdIHw9ICgxOTIgfCBjb2RlID4+PiA2KSA8PCBTSElGVFtpKysgJiAzXTsKICAgICAgICAgICAgICAgIGJsb2NrczJbaSA+Pj4gMl0gfD0gKDEyOCB8IGNvZGUgJiA2MykgPDwgU0hJRlRbaSsrICYgM107CiAgICAgICAgICAgICAgfSBlbHNlIGlmIChjb2RlIDwgNTUyOTYgfHwgY29kZSA+PSA1NzM0NCkgewogICAgICAgICAgICAgICAgYmxvY2tzMltpID4+PiAyXSB8PSAoMjI0IHwgY29kZSA+Pj4gMTIpIDw8IFNISUZUW2krKyAmIDNdOwogICAgICAgICAgICAgICAgYmxvY2tzMltpID4+PiAyXSB8PSAoMTI4IHwgY29kZSA+Pj4gNiAmIDYzKSA8PCBTSElGVFtpKysgJiAzXTsKICAgICAgICAgICAgICAgIGJsb2NrczJbaSA+Pj4gMl0gfD0gKDEyOCB8IGNvZGUgJiA2MykgPDwgU0hJRlRbaSsrICYgM107CiAgICAgICAgICAgICAgfSBlbHNlIHsKICAgICAgICAgICAgICAgIGNvZGUgPSA2NTUzNiArICgoY29kZSAmIDEwMjMpIDw8IDEwIHwgbWVzc2FnZS5jaGFyQ29kZUF0KCsraW5kZXgpICYgMTAyMyk7CiAgICAgICAgICAgICAgICBibG9ja3MyW2kgPj4+IDJdIHw9ICgyNDAgfCBjb2RlID4+PiAxOCkgPDwgU0hJRlRbaSsrICYgM107CiAgICAgICAgICAgICAgICBibG9ja3MyW2kgPj4+IDJdIHw9ICgxMjggfCBjb2RlID4+PiAxMiAmIDYzKSA8PCBTSElGVFtpKysgJiAzXTsKICAgICAgICAgICAgICAgIGJsb2NrczJbaSA+Pj4gMl0gfD0gKDEyOCB8IGNvZGUgPj4+IDYgJiA2MykgPDwgU0hJRlRbaSsrICYgM107CiAgICAgICAgICAgICAgICBibG9ja3MyW2kgPj4+IDJdIHw9ICgxMjggfCBjb2RlICYgNjMpIDw8IFNISUZUW2krKyAmIDNdOwogICAgICAgICAgICAgIH0KICAgICAgICAgICAgfQogICAgICAgICAgfQogICAgICAgICAgdGhpcy5sYXN0Qnl0ZUluZGV4ID0gaTsKICAgICAgICAgIHRoaXMuYnl0ZXMgKz0gaSAtIHRoaXMuc3RhcnQ7CiAgICAgICAgICBpZiAoaSA+PSA2NCkgewogICAgICAgICAgICB0aGlzLmJsb2NrID0gYmxvY2tzMlsxNl07CiAgICAgICAgICAgIHRoaXMuc3RhcnQgPSBpIC0gNjQ7CiAgICAgICAgICAgIHRoaXMuaGFzaCgpOwogICAgICAgICAgICB0aGlzLmhhc2hlZCA9IHRydWU7CiAgICAgICAgICB9IGVsc2UgewogICAgICAgICAgICB0aGlzLnN0YXJ0ID0gaTsKICAgICAgICAgIH0KICAgICAgICB9CiAgICAgICAgaWYgKHRoaXMuYnl0ZXMgPiA0Mjk0OTY3Mjk1KSB7CiAgICAgICAgICB0aGlzLmhCeXRlcyArPSB0aGlzLmJ5dGVzIC8gNDI5NDk2NzI5NiA8PCAwOwogICAgICAgICAgdGhpcy5ieXRlcyA9IHRoaXMuYnl0ZXMgJSA0Mjk0OTY3Mjk2OwogICAgICAgIH0KICAgICAgICByZXR1cm4gdGhpczsKICAgICAgfTsKICAgICAgU2hhMjU2LnByb3RvdHlwZS5maW5hbGl6ZSA9IGZ1bmN0aW9uKCkgewogICAgICAgIGlmICh0aGlzLmZpbmFsaXplZCkgewogICAgICAgICAgcmV0dXJuOwogICAgICAgIH0KICAgICAgICB0aGlzLmZpbmFsaXplZCA9IHRydWU7CiAgICAgICAgdmFyIGJsb2NrczIgPSB0aGlzLmJsb2NrcywgaSA9IHRoaXMubGFzdEJ5dGVJbmRleDsKICAgICAgICBibG9ja3MyWzE2XSA9IHRoaXMuYmxvY2s7CiAgICAgICAgYmxvY2tzMltpID4+PiAyXSB8PSBFWFRSQVtpICYgM107CiAgICAgICAgdGhpcy5ibG9jayA9IGJsb2NrczJbMTZdOwogICAgICAgIGlmIChpID49IDU2KSB7CiAgICAgICAgICBpZiAoIXRoaXMuaGFzaGVkKSB7CiAgICAgICAgICAgIHRoaXMuaGFzaCgpOwogICAgICAgICAgfQogICAgICAgICAgYmxvY2tzMlswXSA9IHRoaXMuYmxvY2s7CiAgICAgICAgICBibG9ja3MyWzE2XSA9IGJsb2NrczJbMV0gPSBibG9ja3MyWzJdID0gYmxvY2tzMlszXSA9IGJsb2NrczJbNF0gPSBibG9ja3MyWzVdID0gYmxvY2tzMls2XSA9IGJsb2NrczJbN10gPSBibG9ja3MyWzhdID0gYmxvY2tzMls5XSA9IGJsb2NrczJbMTBdID0gYmxvY2tzMlsxMV0gPSBibG9ja3MyWzEyXSA9IGJsb2NrczJbMTNdID0gYmxvY2tzMlsxNF0gPSBibG9ja3MyWzE1XSA9IDA7CiAgICAgICAgfQogICAgICAgIGJsb2NrczJbMTRdID0gdGhpcy5oQnl0ZXMgPDwgMyB8IHRoaXMuYnl0ZXMgPj4+IDI5OwogICAgICAgIGJsb2NrczJbMTVdID0gdGhpcy5ieXRlcyA8PCAzOwogICAgICAgIHRoaXMuaGFzaCgpOwogICAgICB9OwogICAgICBTaGEyNTYucHJvdG90eXBlLmhhc2ggPSBmdW5jdGlvbigpIHsKICAgICAgICB2YXIgYSA9IHRoaXMuaDAsIGIgPSB0aGlzLmgxLCBjID0gdGhpcy5oMiwgZCA9IHRoaXMuaDMsIGUgPSB0aGlzLmg0LCBmID0gdGhpcy5oNSwgZyA9IHRoaXMuaDYsIGggPSB0aGlzLmg3LCBibG9ja3MyID0gdGhpcy5ibG9ja3MsIGosIHMwLCBzMSwgbWFqLCB0MSwgdDIsIGNoLCBhYiwgZGEsIGNkLCBiYzsKICAgICAgICBmb3IgKGogPSAxNjsgaiA8IDY0OyArK2opIHsKICAgICAgICAgIHQxID0gYmxvY2tzMltqIC0gMTVdOwogICAgICAgICAgczAgPSAodDEgPj4+IDcgfCB0MSA8PCAyNSkgXiAodDEgPj4+IDE4IHwgdDEgPDwgMTQpIF4gdDEgPj4+IDM7CiAgICAgICAgICB0MSA9IGJsb2NrczJbaiAtIDJdOwogICAgICAgICAgczEgPSAodDEgPj4+IDE3IHwgdDEgPDwgMTUpIF4gKHQxID4+PiAxOSB8IHQxIDw8IDEzKSBeIHQxID4+PiAxMDsKICAgICAgICAgIGJsb2NrczJbal0gPSBibG9ja3MyW2ogLSAxNl0gKyBzMCArIGJsb2NrczJbaiAtIDddICsgczEgPDwgMDsKICAgICAgICB9CiAgICAgICAgYmMgPSBiICYgYzsKICAgICAgICBmb3IgKGogPSAwOyBqIDwgNjQ7IGogKz0gNCkgewogICAgICAgICAgaWYgKHRoaXMuZmlyc3QpIHsKICAgICAgICAgICAgaWYgKHRoaXMuaXMyMjQpIHsKICAgICAgICAgICAgICBhYiA9IDMwMDAzMjsKICAgICAgICAgICAgICB0MSA9IGJsb2NrczJbMF0gLSAxNDEzMjU3ODE5OwogICAgICAgICAgICAgIGggPSB0MSAtIDE1MDA1NDU5OSA8PCAwOwogICAgICAgICAgICAgIGQgPSB0MSArIDI0MTc3MDc3IDw8IDA7CiAgICAgICAgICAgIH0gZWxzZSB7CiAgICAgICAgICAgICAgYWIgPSA3MDQ3NTExMDk7CiAgICAgICAgICAgICAgdDEgPSBibG9ja3MyWzBdIC0gMjEwMjQ0MjQ4OwogICAgICAgICAgICAgIGggPSB0MSAtIDE1MjE0ODY1MzQgPDwgMDsKICAgICAgICAgICAgICBkID0gdDEgKyAxNDM2OTQ1NjUgPDwgMDsKICAgICAgICAgICAgfQogICAgICAgICAgICB0aGlzLmZpcnN0ID0gZmFsc2U7CiAgICAgICAgICB9IGVsc2UgewogICAgICAgICAgICBzMCA9IChhID4+PiAyIHwgYSA8PCAzMCkgXiAoYSA+Pj4gMTMgfCBhIDw8IDE5KSBeIChhID4+PiAyMiB8IGEgPDwgMTApOwogICAgICAgICAgICBzMSA9IChlID4+PiA2IHwgZSA8PCAyNikgXiAoZSA+Pj4gMTEgfCBlIDw8IDIxKSBeIChlID4+PiAyNSB8IGUgPDwgNyk7CiAgICAgICAgICAgIGFiID0gYSAmIGI7CiAgICAgICAgICAgIG1haiA9IGFiIF4gYSAmIGMgXiBiYzsKICAgICAgICAgICAgY2ggPSBlICYgZiBeIH5lICYgZzsKICAgICAgICAgICAgdDEgPSBoICsgczEgKyBjaCArIEtbal0gKyBibG9ja3MyW2pdOwogICAgICAgICAgICB0MiA9IHMwICsgbWFqOwogICAgICAgICAgICBoID0gZCArIHQxIDw8IDA7CiAgICAgICAgICAgIGQgPSB0MSArIHQyIDw8IDA7CiAgICAgICAgICB9CiAgICAgICAgICBzMCA9IChkID4+PiAyIHwgZCA8PCAzMCkgXiAoZCA+Pj4gMTMgfCBkIDw8IDE5KSBeIChkID4+PiAyMiB8IGQgPDwgMTApOwogICAgICAgICAgczEgPSAoaCA+Pj4gNiB8IGggPDwgMjYpIF4gKGggPj4+IDExIHwgaCA8PCAyMSkgXiAoaCA+Pj4gMjUgfCBoIDw8IDcpOwogICAgICAgICAgZGEgPSBkICYgYTsKICAgICAgICAgIG1haiA9IGRhIF4gZCAmIGIgXiBhYjsKICAgICAgICAgIGNoID0gaCAmIGUgXiB+aCAmIGY7CiAgICAgICAgICB0MSA9IGcgKyBzMSArIGNoICsgS1tqICsgMV0gKyBibG9ja3MyW2ogKyAxXTsKICAgICAgICAgIHQyID0gczAgKyBtYWo7CiAgICAgICAgICBnID0gYyArIHQxIDw8IDA7CiAgICAgICAgICBjID0gdDEgKyB0MiA8PCAwOwogICAgICAgICAgczAgPSAoYyA+Pj4gMiB8IGMgPDwgMzApIF4gKGMgPj4+IDEzIHwgYyA8PCAxOSkgXiAoYyA+Pj4gMjIgfCBjIDw8IDEwKTsKICAgICAgICAgIHMxID0gKGcgPj4+IDYgfCBnIDw8IDI2KSBeIChnID4+PiAxMSB8IGcgPDwgMjEpIF4gKGcgPj4+IDI1IHwgZyA8PCA3KTsKICAgICAgICAgIGNkID0gYyAmIGQ7CiAgICAgICAgICBtYWogPSBjZCBeIGMgJiBhIF4gZGE7CiAgICAgICAgICBjaCA9IGcgJiBoIF4gfmcgJiBlOwogICAgICAgICAgdDEgPSBmICsgczEgKyBjaCArIEtbaiArIDJdICsgYmxvY2tzMltqICsgMl07CiAgICAgICAgICB0MiA9IHMwICsgbWFqOwogICAgICAgICAgZiA9IGIgKyB0MSA8PCAwOwogICAgICAgICAgYiA9IHQxICsgdDIgPDwgMDsKICAgICAgICAgIHMwID0gKGIgPj4+IDIgfCBiIDw8IDMwKSBeIChiID4+PiAxMyB8IGIgPDwgMTkpIF4gKGIgPj4+IDIyIHwgYiA8PCAxMCk7CiAgICAgICAgICBzMSA9IChmID4+PiA2IHwgZiA8PCAyNikgXiAoZiA+Pj4gMTEgfCBmIDw8IDIxKSBeIChmID4+PiAyNSB8IGYgPDwgNyk7CiAgICAgICAgICBiYyA9IGIgJiBjOwogICAgICAgICAgbWFqID0gYmMgXiBiICYgZCBeIGNkOwogICAgICAgICAgY2ggPSBmICYgZyBeIH5mICYgaDsKICAgICAgICAgIHQxID0gZSArIHMxICsgY2ggKyBLW2ogKyAzXSArIGJsb2NrczJbaiArIDNdOwogICAgICAgICAgdDIgPSBzMCArIG1hajsKICAgICAgICAgIGUgPSBhICsgdDEgPDwgMDsKICAgICAgICAgIGEgPSB0MSArIHQyIDw8IDA7CiAgICAgICAgICB0aGlzLmNocm9tZUJ1Z1dvcmtBcm91bmQgPSB0cnVlOwogICAgICAgIH0KICAgICAgICB0aGlzLmgwID0gdGhpcy5oMCArIGEgPDwgMDsKICAgICAgICB0aGlzLmgxID0gdGhpcy5oMSArIGIgPDwgMDsKICAgICAgICB0aGlzLmgyID0gdGhpcy5oMiArIGMgPDwgMDsKICAgICAgICB0aGlzLmgzID0gdGhpcy5oMyArIGQgPDwgMDsKICAgICAgICB0aGlzLmg0ID0gdGhpcy5oNCArIGUgPDwgMDsKICAgICAgICB0aGlzLmg1ID0gdGhpcy5oNSArIGYgPDwgMDsKICAgICAgICB0aGlzLmg2ID0gdGhpcy5oNiArIGcgPDwgMDsKICAgICAgICB0aGlzLmg3ID0gdGhpcy5oNyArIGggPDwgMDsKICAgICAgfTsKICAgICAgU2hhMjU2LnByb3RvdHlwZS5oZXggPSBmdW5jdGlvbigpIHsKICAgICAgICB0aGlzLmZpbmFsaXplKCk7CiAgICAgICAgdmFyIGgwID0gdGhpcy5oMCwgaDEgPSB0aGlzLmgxLCBoMiA9IHRoaXMuaDIsIGgzID0gdGhpcy5oMywgaDQgPSB0aGlzLmg0LCBoNSA9IHRoaXMuaDUsIGg2ID0gdGhpcy5oNiwgaDcgPSB0aGlzLmg3OwogICAgICAgIHZhciBoZXggPSBIRVhfQ0hBUlNbaDAgPj4+IDI4ICYgMTVdICsgSEVYX0NIQVJTW2gwID4+PiAyNCAmIDE1XSArIEhFWF9DSEFSU1toMCA+Pj4gMjAgJiAxNV0gKyBIRVhfQ0hBUlNbaDAgPj4+IDE2ICYgMTVdICsgSEVYX0NIQVJTW2gwID4+PiAxMiAmIDE1XSArIEhFWF9DSEFSU1toMCA+Pj4gOCAmIDE1XSArIEhFWF9DSEFSU1toMCA+Pj4gNCAmIDE1XSArIEhFWF9DSEFSU1toMCAmIDE1XSArIEhFWF9DSEFSU1toMSA+Pj4gMjggJiAxNV0gKyBIRVhfQ0hBUlNbaDEgPj4+IDI0ICYgMTVdICsgSEVYX0NIQVJTW2gxID4+PiAyMCAmIDE1XSArIEhFWF9DSEFSU1toMSA+Pj4gMTYgJiAxNV0gKyBIRVhfQ0hBUlNbaDEgPj4+IDEyICYgMTVdICsgSEVYX0NIQVJTW2gxID4+PiA4ICYgMTVdICsgSEVYX0NIQVJTW2gxID4+PiA0ICYgMTVdICsgSEVYX0NIQVJTW2gxICYgMTVdICsgSEVYX0NIQVJTW2gyID4+PiAyOCAmIDE1XSArIEhFWF9DSEFSU1toMiA+Pj4gMjQgJiAxNV0gKyBIRVhfQ0hBUlNbaDIgPj4+IDIwICYgMTVdICsgSEVYX0NIQVJTW2gyID4+PiAxNiAmIDE1XSArIEhFWF9DSEFSU1toMiA+Pj4gMTIgJiAxNV0gKyBIRVhfQ0hBUlNbaDIgPj4+IDggJiAxNV0gKyBIRVhfQ0hBUlNbaDIgPj4+IDQgJiAxNV0gKyBIRVhfQ0hBUlNbaDIgJiAxNV0gKyBIRVhfQ0hBUlNbaDMgPj4+IDI4ICYgMTVdICsgSEVYX0NIQVJTW2gzID4+PiAyNCAmIDE1XSArIEhFWF9DSEFSU1toMyA+Pj4gMjAgJiAxNV0gKyBIRVhfQ0hBUlNbaDMgPj4+IDE2ICYgMTVdICsgSEVYX0NIQVJTW2gzID4+PiAxMiAmIDE1XSArIEhFWF9DSEFSU1toMyA+Pj4gOCAmIDE1XSArIEhFWF9DSEFSU1toMyA+Pj4gNCAmIDE1XSArIEhFWF9DSEFSU1toMyAmIDE1XSArIEhFWF9DSEFSU1toNCA+Pj4gMjggJiAxNV0gKyBIRVhfQ0hBUlNbaDQgPj4+IDI0ICYgMTVdICsgSEVYX0NIQVJTW2g0ID4+PiAyMCAmIDE1XSArIEhFWF9DSEFSU1toNCA+Pj4gMTYgJiAxNV0gKyBIRVhfQ0hBUlNbaDQgPj4+IDEyICYgMTVdICsgSEVYX0NIQVJTW2g0ID4+PiA4ICYgMTVdICsgSEVYX0NIQVJTW2g0ID4+PiA0ICYgMTVdICsgSEVYX0NIQVJTW2g0ICYgMTVdICsgSEVYX0NIQVJTW2g1ID4+PiAyOCAmIDE1XSArIEhFWF9DSEFSU1toNSA+Pj4gMjQgJiAxNV0gKyBIRVhfQ0hBUlNbaDUgPj4+IDIwICYgMTVdICsgSEVYX0NIQVJTW2g1ID4+PiAxNiAmIDE1XSArIEhFWF9DSEFSU1toNSA+Pj4gMTIgJiAxNV0gKyBIRVhfQ0hBUlNbaDUgPj4+IDggJiAxNV0gKyBIRVhfQ0hBUlNbaDUgPj4+IDQgJiAxNV0gKyBIRVhfQ0hBUlNbaDUgJiAxNV0gKyBIRVhfQ0hBUlNbaDYgPj4+IDI4ICYgMTVdICsgSEVYX0NIQVJTW2g2ID4+PiAyNCAmIDE1XSArIEhFWF9DSEFSU1toNiA+Pj4gMjAgJiAxNV0gKyBIRVhfQ0hBUlNbaDYgPj4+IDE2ICYgMTVdICsgSEVYX0NIQVJTW2g2ID4+PiAxMiAmIDE1XSArIEhFWF9DSEFSU1toNiA+Pj4gOCAmIDE1XSArIEhFWF9DSEFSU1toNiA+Pj4gNCAmIDE1XSArIEhFWF9DSEFSU1toNiAmIDE1XTsKICAgICAgICBpZiAoIXRoaXMuaXMyMjQpIHsKICAgICAgICAgIGhleCArPSBIRVhfQ0hBUlNbaDcgPj4+IDI4ICYgMTVdICsgSEVYX0NIQVJTW2g3ID4+PiAyNCAmIDE1XSArIEhFWF9DSEFSU1toNyA+Pj4gMjAgJiAxNV0gKyBIRVhfQ0hBUlNbaDcgPj4+IDE2ICYgMTVdICsgSEVYX0NIQVJTW2g3ID4+PiAxMiAmIDE1XSArIEhFWF9DSEFSU1toNyA+Pj4gOCAmIDE1XSArIEhFWF9DSEFSU1toNyA+Pj4gNCAmIDE1XSArIEhFWF9DSEFSU1toNyAmIDE1XTsKICAgICAgICB9CiAgICAgICAgcmV0dXJuIGhleDsKICAgICAgfTsKICAgICAgU2hhMjU2LnByb3RvdHlwZS50b1N0cmluZyA9IFNoYTI1Ni5wcm90b3R5cGUuaGV4OwogICAgICBTaGEyNTYucHJvdG90eXBlLmRpZ2VzdCA9IGZ1bmN0aW9uKCkgewogICAgICAgIHRoaXMuZmluYWxpemUoKTsKICAgICAgICB2YXIgaDAgPSB0aGlzLmgwLCBoMSA9IHRoaXMuaDEsIGgyID0gdGhpcy5oMiwgaDMgPSB0aGlzLmgzLCBoNCA9IHRoaXMuaDQsIGg1ID0gdGhpcy5oNSwgaDYgPSB0aGlzLmg2LCBoNyA9IHRoaXMuaDc7CiAgICAgICAgdmFyIGFyciA9IFsKICAgICAgICAgIGgwID4+PiAyNCAmIDI1NSwKICAgICAgICAgIGgwID4+PiAxNiAmIDI1NSwKICAgICAgICAgIGgwID4+PiA4ICYgMjU1LAogICAgICAgICAgaDAgJiAyNTUsCiAgICAgICAgICBoMSA+Pj4gMjQgJiAyNTUsCiAgICAgICAgICBoMSA+Pj4gMTYgJiAyNTUsCiAgICAgICAgICBoMSA+Pj4gOCAmIDI1NSwKICAgICAgICAgIGgxICYgMjU1LAogICAgICAgICAgaDIgPj4+IDI0ICYgMjU1LAogICAgICAgICAgaDIgPj4+IDE2ICYgMjU1LAogICAgICAgICAgaDIgPj4+IDggJiAyNTUsCiAgICAgICAgICBoMiAmIDI1NSwKICAgICAgICAgIGgzID4+PiAyNCAmIDI1NSwKICAgICAgICAgIGgzID4+PiAxNiAmIDI1NSwKICAgICAgICAgIGgzID4+PiA4ICYgMjU1LAogICAgICAgICAgaDMgJiAyNTUsCiAgICAgICAgICBoNCA+Pj4gMjQgJiAyNTUsCiAgICAgICAgICBoNCA+Pj4gMTYgJiAyNTUsCiAgICAgICAgICBoNCA+Pj4gOCAmIDI1NSwKICAgICAgICAgIGg0ICYgMjU1LAogICAgICAgICAgaDUgPj4+IDI0ICYgMjU1LAogICAgICAgICAgaDUgPj4+IDE2ICYgMjU1LAogICAgICAgICAgaDUgPj4+IDggJiAyNTUsCiAgICAgICAgICBoNSAmIDI1NSwKICAgICAgICAgIGg2ID4+PiAyNCAmIDI1NSwKICAgICAgICAgIGg2ID4+PiAxNiAmIDI1NSwKICAgICAgICAgIGg2ID4+PiA4ICYgMjU1LAogICAgICAgICAgaDYgJiAyNTUKICAgICAgICBdOwogICAgICAgIGlmICghdGhpcy5pczIyNCkgewogICAgICAgICAgYXJyLnB1c2goaDcgPj4+IDI0ICYgMjU1LCBoNyA+Pj4gMTYgJiAyNTUsIGg3ID4+PiA4ICYgMjU1LCBoNyAmIDI1NSk7CiAgICAgICAgfQogICAgICAgIHJldHVybiBhcnI7CiAgICAgIH07CiAgICAgIFNoYTI1Ni5wcm90b3R5cGUuYXJyYXkgPSBTaGEyNTYucHJvdG90eXBlLmRpZ2VzdDsKICAgICAgU2hhMjU2LnByb3RvdHlwZS5hcnJheUJ1ZmZlciA9IGZ1bmN0aW9uKCkgewogICAgICAgIHRoaXMuZmluYWxpemUoKTsKICAgICAgICB2YXIgYnVmZmVyID0gbmV3IEFycmF5QnVmZmVyKHRoaXMuaXMyMjQgPyAyOCA6IDMyKTsKICAgICAgICB2YXIgZGF0YVZpZXcgPSBuZXcgRGF0YVZpZXcoYnVmZmVyKTsKICAgICAgICBkYXRhVmlldy5zZXRVaW50MzIoMCwgdGhpcy5oMCk7CiAgICAgICAgZGF0YVZpZXcuc2V0VWludDMyKDQsIHRoaXMuaDEpOwogICAgICAgIGRhdGFWaWV3LnNldFVpbnQzMig4LCB0aGlzLmgyKTsKICAgICAgICBkYXRhVmlldy5zZXRVaW50MzIoMTIsIHRoaXMuaDMpOwogICAgICAgIGRhdGFWaWV3LnNldFVpbnQzMigxNiwgdGhpcy5oNCk7CiAgICAgICAgZGF0YVZpZXcuc2V0VWludDMyKDIwLCB0aGlzLmg1KTsKICAgICAgICBkYXRhVmlldy5zZXRVaW50MzIoMjQsIHRoaXMuaDYpOwogICAgICAgIGlmICghdGhpcy5pczIyNCkgewogICAgICAgICAgZGF0YVZpZXcuc2V0VWludDMyKDI4LCB0aGlzLmg3KTsKICAgICAgICB9CiAgICAgICAgcmV0dXJuIGJ1ZmZlcjsKICAgICAgfTsKICAgICAgZnVuY3Rpb24gSG1hY1NoYTI1NihrZXksIGlzMjI0LCBzaGFyZWRNZW1vcnkpIHsKICAgICAgICB2YXIgaSwgdHlwZSA9IHR5cGVvZiBrZXk7CiAgICAgICAgaWYgKHR5cGUgPT09ICJzdHJpbmciKSB7CiAgICAgICAgICB2YXIgYnl0ZXMgPSBbXSwgbGVuZ3RoID0ga2V5Lmxlbmd0aCwgaW5kZXggPSAwLCBjb2RlOwogICAgICAgICAgZm9yIChpID0gMDsgaSA8IGxlbmd0aDsgKytpKSB7CiAgICAgICAgICAgIGNvZGUgPSBrZXkuY2hhckNvZGVBdChpKTsKICAgICAgICAgICAgaWYgKGNvZGUgPCAxMjgpIHsKICAgICAgICAgICAgICBieXRlc1tpbmRleCsrXSA9IGNvZGU7CiAgICAgICAgICAgIH0gZWxzZSBpZiAoY29kZSA8IDIwNDgpIHsKICAgICAgICAgICAgICBieXRlc1tpbmRleCsrXSA9IDE5MiB8IGNvZGUgPj4+IDY7CiAgICAgICAgICAgICAgYnl0ZXNbaW5kZXgrK10gPSAxMjggfCBjb2RlICYgNjM7CiAgICAgICAgICAgIH0gZWxzZSBpZiAoY29kZSA8IDU1Mjk2IHx8IGNvZGUgPj0gNTczNDQpIHsKICAgICAgICAgICAgICBieXRlc1tpbmRleCsrXSA9IDIyNCB8IGNvZGUgPj4+IDEyOwogICAgICAgICAgICAgIGJ5dGVzW2luZGV4KytdID0gMTI4IHwgY29kZSA+Pj4gNiAmIDYzOwogICAgICAgICAgICAgIGJ5dGVzW2luZGV4KytdID0gMTI4IHwgY29kZSAmIDYzOwogICAgICAgICAgICB9IGVsc2UgewogICAgICAgICAgICAgIGNvZGUgPSA2NTUzNiArICgoY29kZSAmIDEwMjMpIDw8IDEwIHwga2V5LmNoYXJDb2RlQXQoKytpKSAmIDEwMjMpOwogICAgICAgICAgICAgIGJ5dGVzW2luZGV4KytdID0gMjQwIHwgY29kZSA+Pj4gMTg7CiAgICAgICAgICAgICAgYnl0ZXNbaW5kZXgrK10gPSAxMjggfCBjb2RlID4+PiAxMiAmIDYzOwogICAgICAgICAgICAgIGJ5dGVzW2luZGV4KytdID0gMTI4IHwgY29kZSA+Pj4gNiAmIDYzOwogICAgICAgICAgICAgIGJ5dGVzW2luZGV4KytdID0gMTI4IHwgY29kZSAmIDYzOwogICAgICAgICAgICB9CiAgICAgICAgICB9CiAgICAgICAgICBrZXkgPSBieXRlczsKICAgICAgICB9IGVsc2UgewogICAgICAgICAgaWYgKHR5cGUgPT09ICJvYmplY3QiKSB7CiAgICAgICAgICAgIGlmIChrZXkgPT09IG51bGwpIHsKICAgICAgICAgICAgICB0aHJvdyBuZXcgRXJyb3IoRVJST1IpOwogICAgICAgICAgICB9IGVsc2UgaWYgKEFSUkFZX0JVRkZFUiAmJiBrZXkuY29uc3RydWN0b3IgPT09IEFycmF5QnVmZmVyKSB7CiAgICAgICAgICAgICAga2V5ID0gbmV3IFVpbnQ4QXJyYXkoa2V5KTsKICAgICAgICAgICAgfSBlbHNlIGlmICghQXJyYXkuaXNBcnJheShrZXkpKSB7CiAgICAgICAgICAgICAgaWYgKCFBUlJBWV9CVUZGRVIgfHwgIUFycmF5QnVmZmVyLmlzVmlldyhrZXkpKSB7CiAgICAgICAgICAgICAgICB0aHJvdyBuZXcgRXJyb3IoRVJST1IpOwogICAgICAgICAgICAgIH0KICAgICAgICAgICAgfQogICAgICAgICAgfSBlbHNlIHsKICAgICAgICAgICAgdGhyb3cgbmV3IEVycm9yKEVSUk9SKTsKICAgICAgICAgIH0KICAgICAgICB9CiAgICAgICAgaWYgKGtleS5sZW5ndGggPiA2NCkgewogICAgICAgICAga2V5ID0gbmV3IFNoYTI1NihpczIyNCwgdHJ1ZSkudXBkYXRlKGtleSkuYXJyYXkoKTsKICAgICAgICB9CiAgICAgICAgdmFyIG9LZXlQYWQgPSBbXSwgaUtleVBhZCA9IFtdOwogICAgICAgIGZvciAoaSA9IDA7IGkgPCA2NDsgKytpKSB7CiAgICAgICAgICB2YXIgYiA9IGtleVtpXSB8fCAwOwogICAgICAgICAgb0tleVBhZFtpXSA9IDkyIF4gYjsKICAgICAgICAgIGlLZXlQYWRbaV0gPSA1NCBeIGI7CiAgICAgICAgfQogICAgICAgIFNoYTI1Ni5jYWxsKHRoaXMsIGlzMjI0LCBzaGFyZWRNZW1vcnkpOwogICAgICAgIHRoaXMudXBkYXRlKGlLZXlQYWQpOwogICAgICAgIHRoaXMub0tleVBhZCA9IG9LZXlQYWQ7CiAgICAgICAgdGhpcy5pbm5lciA9IHRydWU7CiAgICAgICAgdGhpcy5zaGFyZWRNZW1vcnkgPSBzaGFyZWRNZW1vcnk7CiAgICAgIH0KICAgICAgSG1hY1NoYTI1Ni5wcm90b3R5cGUgPSBuZXcgU2hhMjU2KCk7CiAgICAgIEhtYWNTaGEyNTYucHJvdG90eXBlLmZpbmFsaXplID0gZnVuY3Rpb24oKSB7CiAgICAgICAgU2hhMjU2LnByb3RvdHlwZS5maW5hbGl6ZS5jYWxsKHRoaXMpOwogICAgICAgIGlmICh0aGlzLmlubmVyKSB7CiAgICAgICAgICB0aGlzLmlubmVyID0gZmFsc2U7CiAgICAgICAgICB2YXIgaW5uZXJIYXNoID0gdGhpcy5hcnJheSgpOwogICAgICAgICAgU2hhMjU2LmNhbGwodGhpcywgdGhpcy5pczIyNCwgdGhpcy5zaGFyZWRNZW1vcnkpOwogICAgICAgICAgdGhpcy51cGRhdGUodGhpcy5vS2V5UGFkKTsKICAgICAgICAgIHRoaXMudXBkYXRlKGlubmVySGFzaCk7CiAgICAgICAgICBTaGEyNTYucHJvdG90eXBlLmZpbmFsaXplLmNhbGwodGhpcyk7CiAgICAgICAgfQogICAgICB9OwogICAgICB2YXIgZXhwb3J0czIgPSBjcmVhdGVNZXRob2QoKTsKICAgICAgZXhwb3J0czIuc2hhMjU2ID0gZXhwb3J0czI7CiAgICAgIGV4cG9ydHMyLnNoYTIyNCA9IGNyZWF0ZU1ldGhvZCh0cnVlKTsKICAgICAgZXhwb3J0czIuc2hhMjU2LmhtYWMgPSBjcmVhdGVIbWFjTWV0aG9kKCk7CiAgICAgIGV4cG9ydHMyLnNoYTIyNC5obWFjID0gY3JlYXRlSG1hY01ldGhvZCh0cnVlKTsKICAgICAgaWYgKENPTU1PTl9KUykgewogICAgICAgIG1vZHVsZS5leHBvcnRzID0gZXhwb3J0czI7CiAgICAgIH0gZWxzZSB7CiAgICAgICAgcm9vdC5zaGEyNTYgPSBleHBvcnRzMi5zaGEyNTY7CiAgICAgICAgcm9vdC5zaGEyMjQgPSBleHBvcnRzMi5zaGEyMjQ7CiAgICAgICAgaWYgKEFNRCkgewogICAgICAgICAgZGVmaW5lKGZ1bmN0aW9uKCkgewogICAgICAgICAgICByZXR1cm4gZXhwb3J0czI7CiAgICAgICAgICB9KTsKICAgICAgICB9CiAgICAgIH0KICAgIH0pKCk7CiAgfQp9KTsKCi8vIC4uLy4uL3NyYy93b3JrZXJzL3dlYmdsV29ya2VyLndvcmtlci5qcwp2YXIgaW1wb3J0X2pzX3NoYTI1NiA9IF9fdG9FU00ocmVxdWlyZV9zaGEyNTYoKSk7CgovLyAuLi8uLi9zcmMvY29uc3RzL2dsMmZ1bmMuanMKdmFyIGdsMmZ1bmMgPSBbCiAgImNvcHlCdWZmZXJTdWJEYXRhIiwKICAiZ2V0QnVmZmVyU3ViRGF0YSIsCiAgImJsaXRGcmFtZWJ1ZmZlciIsCiAgImZyYW1lYnVmZmVyVGV4dHVyZUxheWVyIiwKICAiZ2V0SW50ZXJuYWxmb3JtYXRQYXJhbWV0ZXIiLAogICJpbnZhbGlkYXRlRnJhbWVidWZmZXIiLAogICJpbnZhbGlkYXRlU3ViRnJhbWVidWZmZXIiLAogICJyZWFkQnVmZmVyIiwKICAicmVuZGVyYnVmZmVyU3RvcmFnZU11bHRpc2FtcGxlIiwKICAidGV4U3RvcmFnZTJEIiwKICAidGV4U3RvcmFnZTNEIiwKICAidGV4SW1hZ2UzRCIsCiAgInRleFN1YkltYWdlM0QiLAogICJjb3B5VGV4U3ViSW1hZ2UzRCIsCiAgImNvbXByZXNzZWRUZXhJbWFnZTNEIiwKICAiY29tcHJlc3NlZFRleFN1YkltYWdlM0QiLAogICJnZXRGcmFnRGF0YUxvY2F0aW9uIiwKICAidW5pZm9ybTF1aSIsCiAgInVuaWZvcm0ydWkiLAogICJ1bmlmb3JtM3VpIiwKICAidW5pZm9ybTR1aSIsCiAgInVuaWZvcm0xdWl2IiwKICAidW5pZm9ybTJ1aXYiLAogICJ1bmlmb3JtM3VpdiIsCiAgInVuaWZvcm00dWl2IiwKICAidW5pZm9ybU1hdHJpeDJ4M2Z2IiwKICAidW5pZm9ybU1hdHJpeDN4MmZ2IiwKICAidW5pZm9ybU1hdHJpeDJ4NGZ2IiwKICAidW5pZm9ybU1hdHJpeDR4MmZ2IiwKICAidW5pZm9ybU1hdHJpeDN4NGZ2IiwKICAidW5pZm9ybU1hdHJpeDR4M2Z2IiwKICAidmVydGV4QXR0cmliSTRpIiwKICAidmVydGV4QXR0cmliSTRpdiIsCiAgInZlcnRleEF0dHJpYkk0dWkiLAogICJ2ZXJ0ZXhBdHRyaWJJNHVpdiIsCiAgInZlcnRleEF0dHJpYklQb2ludGVyIiwKICAidmVydGV4QXR0cmliRGl2aXNvciIsCiAgImRyYXdBcnJheXNJbnN0YW5jZWQiLAogICJkcmF3RWxlbWVudHNJbnN0YW5jZWQiLAogICJkcmF3UmFuZ2VFbGVtZW50cyIsCiAgImRyYXdCdWZmZXJzIiwKICAiY2xlYXJCdWZmZXJpdiIsCiAgImNsZWFyQnVmZmVydWl2IiwKICAiY2xlYXJCdWZmZXJmdiIsCiAgImNsZWFyQnVmZmVyZmkiLAogICJjcmVhdGVRdWVyeSIsCiAgImRlbGV0ZVF1ZXJ5IiwKICAiaXNRdWVyeSIsCiAgImJlZ2luUXVlcnkiLAogICJlbmRRdWVyeSIsCiAgImdldFF1ZXJ5IiwKICAiZ2V0UXVlcnlQYXJhbWV0ZXIiLAogICJjcmVhdGVTYW1wbGVyIiwKICAiZGVsZXRlU2FtcGxlciIsCiAgImlzU2FtcGxlciIsCiAgImJpbmRTYW1wbGVyIiwKICAic2FtcGxlclBhcmFtZXRlcmkiLAogICJzYW1wbGVyUGFyYW1ldGVyZiIsCiAgImdldFNhbXBsZXJQYXJhbWV0ZXIiLAogICJmZW5jZVN5bmMiLAogICJpc1N5bmMiLAogICJkZWxldGVTeW5jIiwKICAiY2xpZW50V2FpdFN5bmMiLAogICJ3YWl0U3luYyIsCiAgImdldFN5bmNQYXJhbWV0ZXIiLAogICJjcmVhdGVUcmFuc2Zvcm1GZWVkYmFjayIsCiAgImRlbGV0ZVRyYW5zZm9ybUZlZWRiYWNrIiwKICAiaXNUcmFuc2Zvcm1GZWVkYmFjayIsCiAgImJpbmRUcmFuc2Zvcm1GZWVkYmFjayIsCiAgImJlZ2luVHJhbnNmb3JtRmVlZGJhY2siLAogICJlbmRUcmFuc2Zvcm1GZWVkYmFjayIsCiAgInRyYW5zZm9ybUZlZWRiYWNrVmFyeWluZ3MiLAogICJnZXRUcmFuc2Zvcm1GZWVkYmFja1ZhcnlpbmciLAogICJwYXVzZVRyYW5zZm9ybUZlZWRiYWNrIiwKICAicmVzdW1lVHJhbnNmb3JtRmVlZGJhY2siLAogICJiaW5kQnVmZmVyQmFzZSIsCiAgImJpbmRCdWZmZXJSYW5nZSIsCiAgImdldEluZGV4ZWRQYXJhbWV0ZXIiLAogICJnZXRVbmlmb3JtSW5kaWNlcyIsCiAgImdldEFjdGl2ZVVuaWZvcm1zIiwKICAiZ2V0VW5pZm9ybUJsb2NrSW5kZXgiLAogICJnZXRBY3RpdmVVbmlmb3JtQmxvY2tQYXJhbWV0ZXIiLAogICJnZXRBY3RpdmVVbmlmb3JtQmxvY2tOYW1lIiwKICAidW5pZm9ybUJsb2NrQmluZGluZyIsCiAgImNyZWF0ZVZlcnRleEFycmF5IiwKICAiZGVsZXRlVmVydGV4QXJyYXkiLAogICJpc1ZlcnRleEFycmF5IiwKICAiYmluZFZlcnRleEFycmF5IgpdOwoKLy8gLi4vLi4vc3JjL3dvcmtlcnMvd2ViZ2xXb3JrZXIud29ya2VyLmpzCmZ1bmN0aW9uIGRlc2NyaWJlUmFuZ2UodikgewogIHJldHVybiBgWyR7dlswXX0sJHt2WzFdfV1gOwp9CmZ1bmN0aW9uIGZvcm1hdFBvd2VyKGUsIHZlcmJvc2UpIHsKICByZXR1cm4gdmVyYm9zZSA/IGAkezIgKiogZX1gIDogYDJeJHtlfWA7Cn0KZnVuY3Rpb24gZ2V0UHJlY2lzaW9uRGVzY3JpcHRpb24ocCwgdmVyYm9zZSkgewogIGNvbnN0IHZwID0gdmVyYm9zZSA/ICIgYml0IG1hbnRpc3NhIiA6ICIiOwogIHJldHVybiBgWy0ke2Zvcm1hdFBvd2VyKHAucmFuZ2VNaW4sIHZlcmJvc2UpfSwke2Zvcm1hdFBvd2VyKHAucmFuZ2VNYXgsIHZlcmJvc2UpfV0oJHtwLnByZWNpc2lvbiArIHZwfSlgOwp9CmZ1bmN0aW9uIGlzUG93ZXJPZlR3byhuKSB7CiAgcmV0dXJuIG4gIT09IDAgJiYgKG4gJiBuIC0gMSkgPT09IDA7Cn0KZnVuY3Rpb24gZ2V0TWFqb3JQZXJmb3JtYW5jZUNhdmVhdChuYW1lKSB7CiAgY29uc3QgYyA9IG5ldyBPZmZzY3JlZW5DYW52YXMoMSwgMSkuZ2V0Q29udGV4dChuYW1lLCB7IGZhaWxJZk1ham9yUGVyZm9ybWFuY2VDYXZlYXQ6IHRydWUgfSk7CiAgcmV0dXJuICFjOwp9CmZ1bmN0aW9uIGdldEFOR0xFKGdsKSB7CiAgY29uc3QgbHcgPSBkZXNjcmliZVJhbmdlKGdsLmdldFBhcmFtZXRlcihnbC5BTElBU0VEX0xJTkVfV0lEVEhfUkFOR0UpKTsKICBjb25zdCBhbmdsZSA9IG5hdmlnYXRvci5wbGF0Zm9ybS5zdGFydHNXaXRoKCJXaW4iKSAmJiBnbC5nZXRQYXJhbWV0ZXIoZ2wuUkVOREVSRVIpICE9PSAiSW50ZXJuZXQgRXhwbG9yZXIiICYmIGdsLmdldFBhcmFtZXRlcihnbC5SRU5ERVJFUikgIT09ICJNaWNyb3NvZnQgRWRnZSIgJiYgbHcgPT09IGRlc2NyaWJlUmFuZ2UoWzEsIDFdKTsKICBpZiAoIWFuZ2xlKQogICAgcmV0dXJuICJObyI7CiAgaWYgKGlzUG93ZXJPZlR3byhnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1ZFUlRFWF9VTklGT1JNX1ZFQ1RPUlMpKSAmJiBpc1Bvd2VyT2ZUd28oZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9GUkFHTUVOVF9VTklGT1JNX1ZFQ1RPUlMpKSkgewogICAgcmV0dXJuICJZZXMsRDNEMTEiOwogIH0KICByZXR1cm4gIlllcyxEM0Q5IjsKfQpmdW5jdGlvbiBnZXRVbm1hc2tlZEluZm8oZ2wpIHsKICBjb25zdCBpbmZvID0geyB2ZW5kb3I6ICIiLCByZW5kZXJlcjogIiIgfTsKICBjb25zdCBkYmcgPSBnbC5nZXRFeHRlbnNpb24oIldFQkdMX2RlYnVnX3JlbmRlcmVyX2luZm8iKTsKICBpZiAoIWRiZykKICAgIHJldHVybiBpbmZvOwogIGluZm8udmVuZG9yID0gZ2wuZ2V0UGFyYW1ldGVyKGRiZy5VTk1BU0tFRF9WRU5ET1JfV0VCR0wpOwogIGluZm8ucmVuZGVyZXIgPSBnbC5nZXRQYXJhbWV0ZXIoZGJnLlVOTUFTS0VEX1JFTkRFUkVSX1dFQkdMKTsKICByZXR1cm4gaW5mbzsKfQpmdW5jdGlvbiBnZXRNYXhDb2xvckJ1ZmZlcnMoZ2wpIHsKICBjb25zdCBleHQgPSBnbC5nZXRFeHRlbnNpb24oIldFQkdMX2RyYXdfYnVmZmVycyIpOwogIHJldHVybiBleHQgPyBnbC5nZXRQYXJhbWV0ZXIoZXh0Lk1BWF9EUkFXX0JVRkZFUlNfV0VCR0wpIDogMTsKfQpmdW5jdGlvbiBnZXRNYXhBbmlzb3Ryb3B5KGdsKSB7CiAgY29uc3QgZXh0ID0gZ2wuZ2V0RXh0ZW5zaW9uKCJFWFRfdGV4dHVyZV9maWx0ZXJfYW5pc290cm9waWMiKSB8fCBnbC5nZXRFeHRlbnNpb24oIldFQktJVF9FWFRfdGV4dHVyZV9maWx0ZXJfYW5pc290cm9waWMiKSB8fCBnbC5nZXRFeHRlbnNpb24oIk1PWl9FWFRfdGV4dHVyZV9maWx0ZXJfYW5pc290cm9waWMiKTsKICBpZiAoIWV4dCkKICAgIHJldHVybiAwOwogIGNvbnN0IG1heCA9IGdsLmdldFBhcmFtZXRlcihleHQuTUFYX1RFWFRVUkVfTUFYX0FOSVNPVFJPUFlfRVhUKTsKICByZXR1cm4gbWF4ID09PSAwID8gMiA6IG1heDsKfQpmdW5jdGlvbiBnZXRCZXN0RmxvYXRQcmVjaXNpb24oZ2wsIHR5cGUpIHsKICBjb25zdCBoaWdoID0gZ2wuZ2V0U2hhZGVyUHJlY2lzaW9uRm9ybWF0KHR5cGUsIGdsLkhJR0hfRkxPQVQpOwogIGNvbnN0IG1lZCA9IGdsLmdldFNoYWRlclByZWNpc2lvbkZvcm1hdCh0eXBlLCBnbC5NRURJVU1fRkxPQVQpOwogIHJldHVybiBnZXRQcmVjaXNpb25EZXNjcmlwdGlvbihoaWdoLnByZWNpc2lvbiA/IGhpZ2ggOiBtZWQsIGZhbHNlKTsKfQpmdW5jdGlvbiBnZXRGbG9hdEludFByZWNpc2lvbihnbCkgewogIGNvbnN0IGhmID0gZ2wuZ2V0U2hhZGVyUHJlY2lzaW9uRm9ybWF0KGdsLkZSQUdNRU5UX1NIQURFUiwgZ2wuSElHSF9GTE9BVCk7CiAgY29uc3QgaGkgPSBnbC5nZXRTaGFkZXJQcmVjaXNpb25Gb3JtYXQoZ2wuRlJBR01FTlRfU0hBREVSLCBnbC5ISUdIX0lOVCk7CiAgcmV0dXJuIGAke2hmLnByZWNpc2lvbiAhPT0gMCA/ICJoaWdocCIgOiAibWVkaXVtcCJ9LyR7aGkucmFuZ2VNYXggIT09IDAgPyAiaGlnaHAiIDogImxvd3AifWA7Cn0KZnVuY3Rpb24gZHJhd0FuZFJlYWRQaXhlbHMoZ2wsIHdpZHRoLCBoZWlnaHQpIHsKICBjb25zdCB2U3JjID0gImF0dHJpYnV0ZSB2ZWMyIGF0dHJWZXJ0ZXg7dmFyeWluZyB2ZWMyIHZhcnlpbmdUZXhDb29yZGluYXRlO3VuaWZvcm0gdmVjMiB1bmlmb3JtT2Zmc2V0O3ZvaWQgbWFpbigpe3ZhcnlpbmdUZXhDb29yZGluYXRlPWF0dHJWZXJ0ZXgrdW5pZm9ybU9mZnNldDtnbF9Qb3NpdGlvbj12ZWM0KGF0dHJWZXJ0ZXgsMCwxKTt9IjsKICBjb25zdCBmU3JjID0gInByZWNpc2lvbiBtZWRpdW1wIGZsb2F0O3ZhcnlpbmcgdmVjMiB2YXJ5aW5nVGV4Q29vcmRpbmF0ZTt2b2lkIG1haW4oKXtnbF9GcmFnQ29sb3I9dmVjNCh2YXJ5aW5nVGV4Q29vcmRpbmF0ZSwwLDEpO30iOwogIGNvbnN0IGJ1ZkRhdGEgPSBuZXcgRmxvYXQzMkFycmF5KFstMC4yLCAtMC45LCAwLCAwLjQsIC0wLjI2LCAwLCAwLCAwLjczMjEzNDQ0NCwgMF0pOwogIGNvbnN0IHZCdWYgPSBnbC5jcmVhdGVCdWZmZXIoKTsKICBnbC5iaW5kQnVmZmVyKGdsLkFSUkFZX0JVRkZFUiwgdkJ1Zik7CiAgZ2wuYnVmZmVyRGF0YShnbC5BUlJBWV9CVUZGRVIsIGJ1ZkRhdGEsIGdsLlNUQVRJQ19EUkFXKTsKICBjb25zdCBwcm9nID0gZ2wuY3JlYXRlUHJvZ3JhbSgpOwogIGNvbnN0IHZTaCA9IGdsLmNyZWF0ZVNoYWRlcihnbC5WRVJURVhfU0hBREVSKTsKICBnbC5zaGFkZXJTb3VyY2UodlNoLCB2U3JjKTsKICBnbC5jb21waWxlU2hhZGVyKHZTaCk7CiAgY29uc3QgZlNoID0gZ2wuY3JlYXRlU2hhZGVyKGdsLkZSQUdNRU5UX1NIQURFUik7CiAgZ2wuc2hhZGVyU291cmNlKGZTaCwgZlNyYyk7CiAgZ2wuY29tcGlsZVNoYWRlcihmU2gpOwogIGdsLmF0dGFjaFNoYWRlcihwcm9nLCB2U2gpOwogIGdsLmF0dGFjaFNoYWRlcihwcm9nLCBmU2gpOwogIGdsLmxpbmtQcm9ncmFtKHByb2cpOwogIGdsLnVzZVByb2dyYW0ocHJvZyk7CiAgY29uc3QgbG9jID0gZ2wuZ2V0QXR0cmliTG9jYXRpb24ocHJvZywgImF0dHJWZXJ0ZXgiKTsKICBjb25zdCBvZmYgPSBnbC5nZXRVbmlmb3JtTG9jYXRpb24ocHJvZywgInVuaWZvcm1PZmZzZXQiKTsKICBnbC5lbmFibGVWZXJ0ZXhBdHRyaWJBcnJheShsb2MpOwogIGdsLnZlcnRleEF0dHJpYlBvaW50ZXIobG9jLCAzLCBnbC5GTE9BVCwgZmFsc2UsIDAsIDApOwogIGdsLnVuaWZvcm0yZihvZmYsIDEsIDEpOwogIGdsLmNsZWFyKGdsLkNPTE9SX0JVRkZFUl9CSVQpOwogIGdsLmRyYXdBcnJheXMoZ2wuVFJJQU5HTEVfU1RSSVAsIDAsIDMpOwogIGNvbnN0IHBpeGVscyA9IG5ldyBVaW50OEFycmF5KHdpZHRoICogaGVpZ2h0ICogNCk7CiAgZ2wucmVhZFBpeGVscygwLCAwLCB3aWR0aCwgaGVpZ2h0LCBnbC5SR0JBLCBnbC5VTlNJR05FRF9CWVRFLCBwaXhlbHMpOwogIHJldHVybiBwaXhlbHMuYnVmZmVyOwp9CnNlbGYub25tZXNzYWdlID0gKHsgZGF0YSB9KSA9PiB7CiAgY29uc3QgeyBpc0dldFNob3J0IH0gPSBkYXRhOwogIGNvbnN0IGNhbnZhcyA9IG5ldyBPZmZzY3JlZW5DYW52YXMoMzAwLCAxNTApOwogIGxldCBnbCA9IG51bGw7CiAgbGV0IHdlYmdsMiA9IGZhbHNlOwogIGxldCB2ZXJzaW9ucyA9ICJfX18iOwogIGdsID0gY2FudmFzLmdldENvbnRleHQoIndlYmdsMiIpOwogIGlmIChnbCkgewogICAgd2ViZ2wyID0gdHJ1ZTsKICAgIHZlcnNpb25zID0gInYxMmUiOwogIH0gZWxzZSB7CiAgICBnbCA9IGNhbnZhcy5nZXRDb250ZXh0KCJ3ZWJnbCIpOwogICAgaWYgKGdsKSB7CiAgICAgIHdlYmdsMiA9IGZhbHNlOwogICAgICB2ZXJzaW9ucyA9ICJ2MV9lIjsKICAgIH0gZWxzZSB7CiAgICAgIGdsID0gY2FudmFzLmdldENvbnRleHQoImV4cGVyaW1lbnRhbC13ZWJnbCIpOwogICAgICBpZiAoZ2wpIHsKICAgICAgICB3ZWJnbDIgPSBmYWxzZTsKICAgICAgICB2ZXJzaW9ucyA9ICJ2X19lIjsKICAgICAgfSBlbHNlIHsKICAgICAgICBzZWxmLnBvc3RNZXNzYWdlKHsgd2ViZ2w6IGZhbHNlLCB3ZWJnbDI6IGZhbHNlLCB2ZXJzaW9uczogIl9fXyIgfSk7CiAgICAgICAgcmV0dXJuOwogICAgICB9CiAgICB9CiAgfQogIGNvbnN0IHdlYmdsID0gISFnbDsKICBjb25zdCBBTkdMRSA9IGdldEFOR0xFKGdsKTsKICBjb25zdCB1bm1hc2tlZCA9IGdldFVubWFza2VkSW5mbyhnbCk7CiAgY29uc3QgYmFzZURhdGEgPSB7CiAgICB3ZWJnbCwKICAgIHdlYmdsMiwKICAgIHZlcnNpb25zLAogICAgd2ViZ2wyZnVuYzogd2ViZ2wyID8gZ2wyZnVuYy5maWx0ZXIoKGZuKSA9PiBmbiBpbiBnbCkuam9pbigiOyIpIDogIiIsCiAgICBhbnRpYWxpYXNpbmc6ICEhZ2wuZ2V0Q29udGV4dEF0dHJpYnV0ZXMoKS5hbnRpYWxpYXMsCiAgICBpc0FuZ2xlOiBBTkdMRSAhPT0gIk5vIiwKICAgIGFuZ2xlOiBBTkdMRSwKICAgIG1ham9yUGVyZm9ybWFuY2VDYXZlYXQ6IGdldE1ham9yUGVyZm9ybWFuY2VDYXZlYXQod2ViZ2wyID8gIndlYmdsMiIgOiAid2ViZ2wiKSwKICAgIHVubWFza2VkVmVuZG9yOiB1bm1hc2tlZC52ZW5kb3IsCiAgICB1bm1hc2tlZFJlbmRlcmVyOiB1bm1hc2tlZC5yZW5kZXJlcgogIH07CiAgaWYgKGlzR2V0U2hvcnQpIHsKICAgIHNlbGYucG9zdE1lc3NhZ2UoYmFzZURhdGEpOwogICAgcmV0dXJuOwogIH0KICBsZXQgZXh0TGlzdCA9ICIiOwogIHRyeSB7CiAgICBleHRMaXN0ID0gZ2wuZ2V0U3VwcG9ydGVkRXh0ZW5zaW9ucygpLmpvaW4oIjsiKTsKICB9IGNhdGNoIHsKICB9CiAgbGV0IGltYWdlSGFzaCA9IG5ldyBVaW50OEFycmF5KCk7CiAgdHJ5IHsKICAgIGNvbnN0IHBpeEJ1ZiA9IGRyYXdBbmRSZWFkUGl4ZWxzKGdsLCBjYW52YXMud2lkdGgsIGNhbnZhcy5oZWlnaHQpOwogICAgaW1hZ2VIYXNoID0gbmV3IFVpbnQ4QXJyYXkoaW1wb3J0X2pzX3NoYTI1Ni5zaGEyNTYuYXJyYXlCdWZmZXIocGl4QnVmKSk7CiAgfSBjYXRjaCB7CiAgfQogIGNvbnN0IHdlYkdMRGF0YSA9IHsKICAgIGdsVmVyOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuVkVSU0lPTiksCiAgICBzaGFkaW5nTGFuZ1ZlcjogZ2wuZ2V0UGFyYW1ldGVyKGdsLlNIQURJTkdfTEFOR1VBR0VfVkVSU0lPTiksCiAgICB2ZW5kb3I6IGdsLmdldFBhcmFtZXRlcihnbC5WRU5ET1IpLAogICAgcmVuZGVyZXI6IGdsLmdldFBhcmFtZXRlcihnbC5SRU5ERVJFUiksCiAgICBtYXhDb2xvckJ1ZmZlcnM6IGdldE1heENvbG9yQnVmZmVycyhnbCksCiAgICBiaXRzUmdiYTogYFske2dsLmdldFBhcmFtZXRlcihnbC5SRURfQklUUyl9LCR7Z2wuZ2V0UGFyYW1ldGVyKGdsLkdSRUVOX0JJVFMpfSwke2dsLmdldFBhcmFtZXRlcihnbC5CTFVFX0JJVFMpfSwke2dsLmdldFBhcmFtZXRlcihnbC5BTFBIQV9CSVRTKX1dYCwKICAgIGRlcHRoU3RlbmNpbEJpdHM6IGBbJHtnbC5nZXRQYXJhbWV0ZXIoZ2wuREVQVEhfQklUUyl9LCR7Z2wuZ2V0UGFyYW1ldGVyKGdsLlNURU5DSUxfQklUUyl9XWAsCiAgICBtYXhSZW5kZXJCdWZmZXJTaXplOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1JFTkRFUkJVRkZFUl9TSVpFKSwKICAgIG1heENvbWJpbmVkVGV4dHVyZUltYWdlVW5pdHM6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfQ09NQklORURfVEVYVFVSRV9JTUFHRV9VTklUUyksCiAgICBtYXhDdWJlTWFwVGV4dHVyZVNpemU6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfQ1VCRV9NQVBfVEVYVFVSRV9TSVpFKSwKICAgIG1heEZyYWdtZW50VW5pZm9ybVZlY3RvcnM6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfRlJBR01FTlRfVU5JRk9STV9WRUNUT1JTKSwKICAgIG1heFRleHR1cmVJbWFnZVVuaXRzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1RFWFRVUkVfSU1BR0VfVU5JVFMpLAogICAgbWF4VGV4dHVyZVNpemU6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfVEVYVFVSRV9TSVpFKSwKICAgIG1heFZhcnlpbmdWZWN0b3JzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1ZBUllJTkdfVkVDVE9SUyksCiAgICBtYXhWZXJ0ZXhBdHRyaWJ1dGVzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1ZFUlRFWF9BVFRSSUJTKSwKICAgIG1heFZlcnRleFRleHR1cmVJbWFnZVVuaXRzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1ZFUlRFWF9URVhUVVJFX0lNQUdFX1VOSVRTKSwKICAgIG1heFZlcnRleFVuaWZvcm1WZWN0b3JzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1ZFUlRFWF9VTklGT1JNX1ZFQ1RPUlMpLAogICAgYWxpYXNlZExpbmVXaWR0aFJhbmdlOiBkZXNjcmliZVJhbmdlKGdsLmdldFBhcmFtZXRlcihnbC5BTElBU0VEX0xJTkVfV0lEVEhfUkFOR0UpKSwKICAgIGFsaWFzZWRQb2ludFNpemVSYW5nZTogZGVzY3JpYmVSYW5nZShnbC5nZXRQYXJhbWV0ZXIoZ2wuQUxJQVNFRF9QT0lOVF9TSVpFX1JBTkdFKSksCiAgICBtYXhWaWV3cG9ydERpbWVuc2lvbnM6IGRlc2NyaWJlUmFuZ2UoZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9WSUVXUE9SVF9ESU1TKSksCiAgICBtYXhBbmlzb3Ryb3B5OiBnZXRNYXhBbmlzb3Ryb3B5KGdsKSwKICAgIHZlcnRleFNoYWRlckJlc3RQcmVjaXNpb246IGdldEJlc3RGbG9hdFByZWNpc2lvbihnbCwgZ2wuVkVSVEVYX1NIQURFUiksCiAgICBmcmFnbWVudFNoYWRlckJlc3RQcmVjaXNpb246IGdldEJlc3RGbG9hdFByZWNpc2lvbihnbCwgZ2wuRlJBR01FTlRfU0hBREVSKSwKICAgIGZyYWdtZW50U2hhZGVyRmxvYXRJbnRQcmVjaXNpb246IGdldEZsb2F0SW50UHJlY2lzaW9uKGdsKQogIH07CiAgbGV0IHdlYkdMMkRhdGEgPSB7fTsKICBpZiAod2ViZ2wyKSB7CiAgICB3ZWJHTDJEYXRhID0gewogICAgICBtYXhWZXJ0ZXhVbmlmb3JtQ29tcG9uZW50czogZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9WRVJURVhfVU5JRk9STV9DT01QT05FTlRTKSwKICAgICAgbWF4VmVydGV4VW5pZm9ybUJsb2NrczogZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9WRVJURVhfVU5JRk9STV9CTE9DS1MpLAogICAgICBtYXhWZXJ0ZXhPdXRwdXRDb21wb25lbnRzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1ZFUlRFWF9PVVRQVVRfQ09NUE9ORU5UUyksCiAgICAgIG1heFZhcnlpbmdDb21wb25lbnRzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1ZBUllJTkdfQ09NUE9ORU5UUyksCiAgICAgIG1heEZyYWdtZW50VW5pZm9ybUNvbXBvbmVudHM6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfRlJBR01FTlRfVU5JRk9STV9DT01QT05FTlRTKSwKICAgICAgbWF4RnJhZ21lbnRVbmlmb3JtQmxvY2tzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX0ZSQUdNRU5UX1VOSUZPUk1fQkxPQ0tTKSwKICAgICAgbWF4RnJhZ21lbnRJbnB1dENvbXBvbmVudHM6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfRlJBR01FTlRfSU5QVVRfQ09NUE9ORU5UUyksCiAgICAgIG1pblByb2dyYW1UZXhlbE9mZnNldDogZ2wuZ2V0UGFyYW1ldGVyKGdsLk1JTl9QUk9HUkFNX1RFWEVMX09GRlNFVCksCiAgICAgIG1heFByb2dyYW1UZXhlbE9mZnNldDogZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9QUk9HUkFNX1RFWEVMX09GRlNFVCksCiAgICAgIG1heERyYXdCdWZmZXJzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX0RSQVdfQlVGRkVSUyksCiAgICAgIG1heENvbG9yQXR0YWNobWVudHM6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfQ09MT1JfQVRUQUNITUVOVFMpLAogICAgICBtYXhTYW1wbGVzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1NBTVBMRVMpLAogICAgICBtYXgzZFRleHR1cmVTaXplOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYXzNEX1RFWFRVUkVfU0laRSksCiAgICAgIG1heEFycmF5VGV4dHVyZUxheWVyczogZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9BUlJBWV9URVhUVVJFX0xBWUVSUyksCiAgICAgIG1heFVuaWZvcm1CdWZmZXJCaW5kaW5nczogZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9VTklGT1JNX0JVRkZFUl9CSU5ESU5HUyksCiAgICAgIG1heFVuaWZvcm1CbG9ja1NpemU6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfVU5JRk9STV9CTE9DS19TSVpFKSwKICAgICAgdW5pZm9ybUJ1ZmZlck9mZnNldEFsaWdubWVudDogZ2wuZ2V0UGFyYW1ldGVyKGdsLlVOSUZPUk1fQlVGRkVSX09GRlNFVF9BTElHTk1FTlQpLAogICAgICBtYXhDb21iaW5lZFVuaWZvcm1CbG9ja3M6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfQ09NQklORURfVU5JRk9STV9CTE9DS1MpLAogICAgICBtYXhDb21iaW5lZFZlcnRleFVuaWZvcm1Db21wb25lbnRzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX0NPTUJJTkVEX1ZFUlRFWF9VTklGT1JNX0NPTVBPTkVOVFMpLAogICAgICBtYXhDb21iaW5lZEZyYWdtZW50VW5pZm9ybUNvbXBvbmVudHM6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfQ09NQklORURfRlJBR01FTlRfVU5JRk9STV9DT01QT05FTlRTKSwKICAgICAgbWF4VHJhbnNmb3JtRmVlZGJhY2tJbnRlcmxlYXZlZENvbXBvbmVudHM6IGdsLmdldFBhcmFtZXRlcihnbC5NQVhfVFJBTlNGT1JNX0ZFRURCQUNLX0lOVEVSTEVBVkVEX0NPTVBPTkVOVFMpLAogICAgICBtYXhUcmFuc2Zvcm1GZWVkYmFja1NlcGFyYXRlQXR0cmliczogZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9UUkFOU0ZPUk1fRkVFREJBQ0tfU0VQQVJBVEVfQVRUUklCUyksCiAgICAgIG1heFRyYW5zZm9ybUZlZWRiYWNrU2VwYXJhdGVDb21wb25lbnRzOiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX1RSQU5TRk9STV9GRUVEQkFDS19TRVBBUkFURV9DT01QT05FTlRTKSwKICAgICAgbWF4RWxlbWVudEluZGV4OiBnbC5nZXRQYXJhbWV0ZXIoZ2wuTUFYX0VMRU1FTlRfSU5ERVgpLAogICAgICBtYXhTZXJ2ZXJXYWl0VGltZW91dDogZ2wuZ2V0UGFyYW1ldGVyKGdsLk1BWF9TRVJWRVJfV0FJVF9USU1FT1VUKQogICAgfTsKICB9CiAgY29uc3QgZnVsbCA9IHsKICAgIC4uLmJhc2VEYXRhLAogICAgc3VwcG9ydGVkRXh0OiBleHRMaXN0LAogICAgaW1hZ2VIYXNoLAogICAgLi4ud2ViR0xEYXRhLAogICAgLi4ud2ViR0wyRGF0YQogIH07CiAgY29uc3QgcmVwb3J0SGFzaCA9IG5ldyBVaW50OEFycmF5KGltcG9ydF9qc19zaGEyNTYuc2hhMjU2LmFycmF5QnVmZmVyKEpTT04uc3RyaW5naWZ5KGZ1bGwpKSk7CiAgc2VsZi5wb3N0TWVzc2FnZSgKICAgIHsKICAgICAgLi4uZnVsbCwKICAgICAgcmVwb3J0SGFzaAogICAgfSwKICAgIFtpbWFnZUhhc2guYnVmZmVyLCByZXBvcnRIYXNoLmJ1ZmZlcl0KICApOwp9OwovKiEgQnVuZGxlZCBsaWNlbnNlIGluZm9ybWF0aW9uOgoKanMtc2hhMjU2L3NyYy9zaGEyNTYuanM6CiAgKCoqCiAgICogW2pzLXNoYTI1Nl17QGxpbmsgaHR0cHM6Ly9naXRodWIuY29tL2VtbjE3OC9qcy1zaGEyNTZ9CiAgICoKICAgKiBAdmVyc2lvbiAwLjExLjEKICAgKiBAYXV0aG9yIENoZW4sIFlpLUN5dWFuIFtlbW4xNzhAZ21haWwuY29tXQogICAqIEBjb3B5cmlnaHQgQ2hlbiwgWWktQ3l1YW4gMjAxNC0yMDI1CiAgICogQGxpY2Vuc2UgTUlUCiAgICopCiovCg==";

        let parsed = SourceMapUrl::parse_with_prefix("", url).unwrap();
        dbg!(parsed);
    }
}
