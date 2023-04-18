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

use std::collections::{BTreeSet, HashMap};
use std::fmt::{self, Write};
use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;

use data_encoding::BASE64;
use futures::future::BoxFuture;
use reqwest::Url;
use sha2::{Digest, Sha256};
use sourcemap::locate_sourcemap_reference;
use symbolic::common::{ByteView, DebugId, SelfCell};
use symbolic::debuginfo::sourcebundle::{
    SourceBundleDebugSession, SourceFileDescriptor, SourceFileType,
};
use symbolic::debuginfo::Object;
use symbolic::sourcemapcache::{SourceMapCache, SourceMapCacheWriter};
use symbolicator_sources::{
    HttpRemoteFile, ObjectType, RemoteFile, RemoteFileUri, SentrySourceConfig,
};
use tempfile::NamedTempFile;
use url::Position;

use crate::caching::{CacheEntry, CacheError, CacheItemRequest, CacheKey, CacheVersions, Cacher};
use crate::services::download::DownloadService;
use crate::services::objects::ObjectMetaHandle;
use crate::types::{JsStacktrace, Scope};

use super::caches::versions::SOURCEMAP_CACHE_VERSIONS;
use super::caches::{ByteViewString, SourceFilesCache};
use super::download::sentry::{ArtifactHeaders, JsLookupResult};
use super::objects::ObjectsActor;
use super::sourcemap::SourceMapService;
use super::symbolication::SymbolicateJsStacktraces;

pub type OwnedSourceMapCache = SelfCell<ByteView<'static>, SourceMapCache<'static>>;

/// A JS-processing "Module".
///
/// This is basically a single file (identified by its `abs_path`), with some additional metadata
/// about it.
#[derive(Clone, Debug)]
pub struct SourceMapModule {
    /// The parsed [`Url`] or the original `abs_path` along with a [`url::ParseError`] if it is invalid.
    abs_path: Result<Url, (String, url::ParseError)>,
    /// The optional [`DebugId`] of this module.
    debug_id: Option<DebugId>,
    // TODO(sourcemap): errors that happened when processing this file
    /// A flag showing if we have already resolved the minified and sourcemap files.
    was_fetched: bool,
    /// The base url for fetching source files.
    source_file_base: Option<Url>,
    /// The fetched minified JS file.
    // TODO(sourcemap): maybe this should not be public?
    pub minified_source: CachedFileEntry,
    /// The converted SourceMap.
    // TODO(sourcemap): maybe this should not be public?
    pub smcache: Option<CachedFileEntry<OwnedSourceMapCache>>,
}

impl SourceMapModule {
    fn new(abs_path: &str, debug_id: Option<DebugId>) -> Self {
        let abs_path = Url::parse(abs_path).map_err(|err| {
            let error: &dyn std::error::Error = &err;
            tracing::warn!(error, abs_path, "Invalid Url in JS processing");
            (abs_path.to_owned(), err)
        });
        Self {
            abs_path,
            debug_id,
            was_fetched: false,
            source_file_base: None,
            minified_source: CachedFileEntry::empty(),
            smcache: None,
        }
    }

    // TODO(sourcemap): we should really maintain a list of all the errors that happened for this image?
    pub fn is_valid(&self) -> bool {
        self.abs_path.is_ok()
    }

    /// Creates a new [`FileKey`] for the `file_path` relative to this module
    pub fn source_file_key(&self, file_path: &str) -> Option<FileKey> {
        let base_url = self.source_file_base.as_ref()?;
        let url = base_url.join(file_path).ok()?;
        Some(FileKey::new_source(url))
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
            allow_scraping,
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
            allow_scraping,

            artifact_bundles: Default::default(),
            individual_artifacts: Default::default(),

            api_requests: 0,
            queried_artifacts: 0,
            fetched_artifacts: 0,
            queried_bundles: 0,
            scraped_files: 0,
        };

        Self {
            modules_by_abs_path,
            files_by_key: Default::default(),
            fetcher,
        }
    }

    /// Prepares the modules for processing
    pub fn prepare_modules(&mut self, stacktraces: &[JsStacktrace]) {
        for stacktrace in stacktraces {
            for frame in &stacktrace.frames {
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

        let Ok(url) = module.abs_path.clone() else {
            return module;
        };

        // we can’t have a mutable `module` while calling `fetch_module` :-(
        let (minified_source, smcache) = self
            .fetcher
            .fetch_minified_and_sourcemap(url.clone(), module.debug_id)
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
        let source_file_base = sourcemap_url.unwrap_or(url);

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

/// Joins a `url` to the `base` [`Url`], taking care of our special `~/` prefix that is treated just
/// like an absolute url.
fn join_url(base: &Url, url: &str) -> Option<Url> {
    let url = url.strip_prefix('~').unwrap_or(url);
    base.join(url).ok()
}

/// A URL to a sourcemap file.
///
/// May either be a conventional URL or a data URL containing the sourcemap
/// encoded as BASE64.
#[derive(Clone)]
enum SourceMapUrl {
    Data(ByteViewString),
    Remote(url::Url),
}

impl fmt::Debug for SourceMapUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data(data) => {
                let contents: &str = data;
                f.debug_tuple("Data").field(&contents).finish()
            }
            Self::Remote(url) => f.debug_tuple("Remote").field(&url.as_str()).finish(),
        }
    }
}

impl SourceMapUrl {
    /// The string prefix denoting a data URL.
    const DATA_PREAMBLE: &str = "data:application/json;base64,";

    /// Parses a string into a [`SourceMapUrl`].
    ///
    /// If the string starts with [`DATA_PREAMBLE`](Self::DATA_PREAMBLE), the rest is decoded from BASE64.
    /// Otherwise, the string is joined to the `base` URL.
    fn parse_with_prefix(base: &Url, url_string: &str) -> CacheEntry<Self> {
        if let Some(encoded) = url_string.strip_prefix(Self::DATA_PREAMBLE) {
            let decoded = BASE64
                .decode(encoded.as_bytes())
                .map_err(|_| CacheError::Malformed("Invalid base64 sourcemap".to_string()))?;
            let decoded = String::from_utf8(decoded)
                .map_err(|_| CacheError::Malformed("Invalid base64 sourcemap".to_string()))?;
            Ok(Self::Data(decoded.into()))
        } else {
            let url = join_url(base, url_string)
                .ok_or_else(|| CacheError::DownloadError("Invalid sourcemap url".to_string()))?;
            Ok(Self::Remote(url))
        }
    }
}

type ArtifactBundle = SelfCell<ByteView<'static>, SourceBundleDebugSession<'static>>;

/// The lookup key of an arbitrary file.
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub enum FileKey {
    /// This key represents a [`SourceFileType::MinifiedSource`].
    MinifiedSource {
        abs_path: Url,
        debug_id: Option<DebugId>,
    },
    /// This key represents a [`SourceFileType::SourceMap`].
    SourceMap {
        abs_path: Option<Url>,
        debug_id: Option<DebugId>,
    },
    /// This key represents a [`SourceFileType::Source`].
    Source { abs_path: Url },
}

impl FileKey {
    /// Creates a new [`FileKey`] for a source file.
    fn new_source(abs_path: Url) -> Self {
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
    pub fn abs_path(&self) -> Option<&Url> {
        match self {
            FileKey::MinifiedSource { abs_path, .. } => Some(abs_path),
            FileKey::SourceMap { abs_path, .. } => abs_path.as_ref(),
            FileKey::Source { abs_path } => Some(abs_path),
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
    /// The file was fetched using its own URI.
    IndividualFile(RemoteFileUri),
    /// The file was found using [`FileKey`] in the bundle identified by the URI.
    Bundled(RemoteFileUri, FileKey),
    /// The file was embedded in another file. This will only ever happen
    /// for Base64-encoded SourceMaps, and the SourceMap is always used
    /// in combination with a minified File that has a [`CachedFileUri`] itself.
    Embedded,
}

#[derive(Clone, Debug)]
pub struct CachedFileEntry<T = CachedFile> {
    pub uri: CachedFileUri,
    pub entry: CacheEntry<T>,
}

impl<T> CachedFileEntry<T> {
    fn empty() -> Self {
        let uri = RemoteFileUri::new("<invalid>");
        Self {
            uri: CachedFileUri::IndividualFile(uri),
            entry: Err(CacheError::NotFound),
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
        let contents: &str = &self.contents;
        f.debug_struct("CachedFile")
            .field("contents", &contents)
            .field("sourcemap_url", &self.sourcemap_url)
            .finish()
    }
}

impl CachedFile {
    fn from_descriptor(
        abs_path: Option<&Url>,
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

        // TODO(sourcemap): a `into_contents` would be nice, as we are creating a new copy right now
        let contents = descriptor
            .contents()
            .ok_or_else(|| CacheError::Malformed("descriptor should have `contents`".into()))?
            .to_owned();
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
    allow_scraping: bool,

    /// The set of all the artifact bundles that we have downloaded so far.
    artifact_bundles: HashMap<RemoteFileUri, CacheEntry<ArtifactBundle>>,
    /// The set of individual artifacts, by their `url`.
    individual_artifacts: HashMap<String, IndividualArtifact>,

    // various metrics:
    api_requests: u64,
    queried_artifacts: u64,
    fetched_artifacts: u64,
    queried_bundles: u64,
    scraped_files: u64,
}

impl ArtifactFetcher {
    /// Fetches the minified file, and the corresponding [`OwnedSourceMapCache`] for the file
    /// identified by its `abs_path`, or optionally its [`DebugId`].
    #[tracing::instrument(skip(self, abs_path), fields(%abs_path))]
    async fn fetch_minified_and_sourcemap(
        &mut self,
        abs_path: Url,
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
            },
            // We do have a valid `sourceMappingURL`
            Some(SourceMapUrl::Remote(url)) => {
                let sourcemap_key = FileKey::SourceMap {
                    abs_path: Some(url.clone()),
                    debug_id,
                };
                self.get_file(&sourcemap_key).await
            }
            // We have a `DebugId`, in which case we don’t need no URL
            None => {
                let sourcemap_key = FileKey::SourceMap {
                    abs_path: None,
                    debug_id,
                };
                self.get_file(&sourcemap_key).await
            }
        };

        // Now that we (may) have both files, we can create a `SourceMapCache` for it
        let smcache = match &minified_source.entry {
            Ok(minified_source) => match sourcemap.entry {
                Ok(sourcemap) => {
                    self.fetch_sourcemap_cache(minified_source.contents.clone(), sourcemap.contents)
                        .await
                }
                Err(err) => Err(err),
            },
            Err(err) => Err(err.clone()),
        };
        let smcache = CachedFileEntry {
            uri: sourcemap.uri,
            entry: smcache,
        };

        (minified_source, Some(smcache))
    }

    /// Fetches an arbitrary file using its `abs_path`,
    /// or optionally its [`DebugId`] and [`SourceFileType`]
    /// (because multiple files can share one [`DebugId`]).
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
        self.query_sentry_for_file(key).await;

        // At this point, *one* of our known artifacts includes the file we are looking for.
        // So we do the whole dance yet again.
        // TODO(sourcemap): figure out a way to avoid that?
        if let Some(file) = self.try_get_file_from_bundles(key) {
            return file;
        }
        if let Some(file) = self.try_fetch_file_from_artifacts(key).await {
            return file;
        }

        // Otherwise, fall back to scraping from the Web.
        if self.allow_scraping {
            if let Some(url) = key.abs_path() {
                self.scraped_files += 1;
                let remote_file: RemoteFile = HttpRemoteFile::from_url(url.to_owned()).into();
                let uri = CachedFileUri::IndividualFile(remote_file.uri());
                let scraped_file = self
                    .sourcefiles_cache
                    .fetch_file(&self.scope, remote_file)
                    .await;

                return CachedFileEntry {
                    uri,
                    entry: scraped_file.map(|contents| {
                        tracing::trace!(?key, "Found file by scraping the web");

                        let sm_ref = locate_sourcemap_reference(contents.as_bytes())
                            .ok()
                            .flatten();
                        let sourcemap_url = sm_ref.and_then(|sm_ref| {
                            SourceMapUrl::parse_with_prefix(url, sm_ref.get_url())
                                .ok()
                                .map(Arc::new)
                        });

                        CachedFile {
                            contents,
                            sourcemap_url,
                        }
                    }),
                };
            }
        }

        CachedFileEntry::empty()
    }

    fn try_get_file_from_bundles(&self, key: &FileKey) -> Option<CachedFileEntry> {
        // If we have a `DebugId`, we try a lookup based on that.
        if let Some(debug_id) = key.debug_id() {
            let ty = key.as_type();
            for (bundle_uri, bundle) in &self.artifact_bundles {
                let Ok(bundle) = bundle else { continue; };
                let bundle = bundle.get();
                if let Ok(Some(descriptor)) = bundle.source_by_debug_id(debug_id, ty) {
                    tracing::trace!(?key, "Found file in artifact bundles by debug-id");
                    return Some(CachedFileEntry {
                        uri: CachedFileUri::Bundled(bundle_uri.clone(), key.clone()),
                        entry: CachedFile::from_descriptor(key.abs_path(), descriptor),
                    });
                }
            }
        }

        // Otherwise, try all the candidate `abs_path` patterns in every artifact bundle.
        if let Some(abs_path) = key.abs_path() {
            for url in get_release_file_candidate_urls(abs_path) {
                for (bundle_uri, bundle) in &self.artifact_bundles {
                    let Ok(bundle) = bundle else { continue; };
                    let bundle = bundle.get();
                    if let Ok(Some(descriptor)) = bundle.source_by_url(&url) {
                        tracing::trace!(?key, url, "Found file in artifact bundles by url");
                        return Some(CachedFileEntry {
                            uri: CachedFileUri::Bundled(bundle_uri.clone(), key.clone()),
                            entry: CachedFile::from_descriptor(Some(abs_path), descriptor),
                        });
                    }
                }
            }
        }

        None
    }

    async fn try_fetch_file_from_artifacts(&mut self, key: &FileKey) -> Option<CachedFileEntry> {
        let abs_path = key.abs_path()?;
        let (url, artifact) = get_release_file_candidate_urls(abs_path).find_map(|url| {
            self.individual_artifacts
                .get(&url)
                .map(|artifact| (url, artifact))
        })?;

        self.fetched_artifacts += 1;

        let artifact_contents = self
            .sourcefiles_cache
            .fetch_file(&self.scope, artifact.remote_file.clone())
            .await;

        Some(CachedFileEntry {
            uri: CachedFileUri::IndividualFile(artifact.remote_file.uri()),
            entry: artifact_contents.map(|contents| {
                tracing::trace!(?key, ?url, ?artifact, "Found file as individual artifact");

                // Get the sourcemap reference from the artifact, either from metadata, or file contents
                let sourcemap_url =
                    resolve_sourcemap_url(abs_path, &artifact.headers, contents.as_bytes());
                CachedFile {
                    contents,
                    sourcemap_url: sourcemap_url.map(Arc::new),
                }
            }),
        })
    }

    /// Queries the Sentry API for a single file (by its [`DebugId`] and file stem).
    async fn query_sentry_for_file(&mut self, key: &FileKey) {
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
    async fn query_sentry_for_files(
        &mut self,
        debug_ids: BTreeSet<DebugId>,
        file_stems: BTreeSet<String>,
    ) {
        if debug_ids.is_empty() {
            // `file_stems` only make sense in combination with a `release`.
            if file_stems.is_empty() || self.release.is_none() {
                // FIXME: this should really not happen, but I just observed it.
                // The callers should better validate the args in that case?
                return;
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
                return;
            }
        };

        for file in results {
            match file {
                JsLookupResult::IndividualArtifact {
                    remote_file,
                    abs_path,
                    headers,
                } => {
                    self.queried_artifacts += 1;
                    self.individual_artifacts.insert(
                        abs_path,
                        IndividualArtifact {
                            remote_file,
                            headers,
                        },
                    );
                }
                JsLookupResult::ArtifactBundle { remote_file } => {
                    self.queried_bundles += 1;
                    let uri = remote_file.uri();
                    // clippy, you are wrong, as this would result in borrowing errors
                    #[allow(clippy::map_entry)]
                    if !self.artifact_bundles.contains_key(&uri) {
                        // NOTE: This could potentially be done concurrently, but lets not
                        // prematurely optimize for now
                        let artifact_bundle = self.fetch_artifact_bundle(remote_file).await;
                        self.artifact_bundles.insert(uri, artifact_bundle);
                    }
                }
            }
        }
    }

    #[tracing::instrument(skip(self))]
    async fn fetch_artifact_bundle(&self, file: RemoteFile) -> CacheEntry<ArtifactBundle> {
        let object_handle = ObjectMetaHandle::for_scoped_file(self.scope.clone(), file);

        let fetched_bundle = self.objects.fetch(object_handle).await?;
        let owner = fetched_bundle.data().clone();

        SelfCell::try_new(owner, |p| unsafe {
            // We already have a parsed `Object`, but because of ownership issues, we have to parse it again
            match Object::parse(&*p).map_err(CacheError::from_std_error)? {
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
        source: ByteViewString,
        sourcemap: ByteViewString,
    ) -> CacheEntry<OwnedSourceMapCache> {
        let cache_key = {
            let mut cache_key = CacheKey::scoped_builder(&self.scope);
            let source_hash = Sha256::digest(&source);
            let sourcemap_hash = Sha256::digest(&sourcemap);

            write!(cache_key, "source:\n{source_hash:x}\n").unwrap();
            write!(cache_key, "sourcemap:\n{sourcemap_hash:x}").unwrap();

            cache_key.build()
        };

        let req = FetchSourceMapCacheInternal { source, sourcemap };
        self.sourcemap_caches.compute_memoized(req, cache_key).await
    }

    fn record_metrics(&self) {
        metric!(time_raw("js.api_requests") = self.api_requests);
        metric!(time_raw("js.queried_bundles") = self.queried_bundles);
        metric!(time_raw("js.fetched_bundles") = self.artifact_bundles.len() as u64);
        metric!(time_raw("js.queried_artifacts") = self.queried_artifacts);
        metric!(time_raw("js.fetched_artifacts") = self.fetched_artifacts);
        metric!(time_raw("js.scraped_files") = self.scraped_files);
    }
}

/// Extracts a "file stem" from a [`Url`].
/// This is the `"/path/to/file"` in `"./path/to/file.min.js?foo=bar"`.
/// We use the most generic variant instead here, as server-side filtering is using a partial
/// match on the whole artifact path, thus `index.js` will be fetched no matter it's stored
/// as `~/index.js`, `~/index.js?foo=bar`, `http://example.com/index.js`,
/// or `http://example.com/index.js?foo=bar`.
// NOTE: We do want a leading slash to be included, eg. `/bundle/app.js` or `/index.js`,
// as it's not possible to use artifacts without proper host or `~/` wildcard.
fn extract_file_stem(url: &Url) -> String {
    let path = url.path();
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
fn get_release_file_candidate_urls(url: &Url) -> impl Iterator<Item = String> {
    let mut urls = [None, None, None, None];

    // Absolute without fragment
    urls[0] = Some(url[..Position::AfterQuery].to_string());

    // Absolute without query
    if url.query().is_some() {
        urls[1] = Some(url[..Position::AfterPath].to_string());
    }

    // Relative without fragment
    urls[2] = Some(format!(
        "~{}",
        &url[Position::BeforePath..Position::AfterQuery]
    ));

    // Relative without query
    if url.query().is_some() {
        urls[3] = Some(format!(
            "~{}",
            &url[Position::BeforePath..Position::AfterPath]
        ));
    }

    urls.into_iter().flatten()
}

/// Joins together frames `abs_path` and discovered sourcemap reference.
fn resolve_sourcemap_url(
    abs_path: &Url,
    artifact_headers: &ArtifactHeaders,
    artifact_source: &[u8],
) -> Option<SourceMapUrl> {
    if let Some(header) = artifact_headers.get("Sourcemap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else if let Some(header) = artifact_headers.get("X-SourceMap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else {
        let sm_ref = locate_sourcemap_reference(artifact_source).ok()??;
        SourceMapUrl::parse_with_prefix(abs_path, sm_ref.get_url()).ok()
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

#[tracing::instrument(skip_all)]
fn write_artifact_cache(file: &mut File, source_artifact_buf: &str) -> CacheEntry {
    use std::io::Write;

    let mut writer = BufWriter::new(file);
    writer.write_all(source_artifact_buf.as_bytes())?;
    let file = writer.into_inner().map_err(io::Error::from)?;
    file.sync_all()?;

    Ok(())
}

fn parse_sourcemap_cache_owned(byteview: ByteView<'static>) -> CacheEntry<OwnedSourceMapCache> {
    SelfCell::try_new(byteview, |p| unsafe {
        SourceMapCache::parse(&*p).map_err(CacheError::from_std_error)
    })
}

/// Computes and writes the SourceMapCache.
#[tracing::instrument(skip_all)]
fn write_sourcemap_cache(file: &mut File, source: &str, sourcemap: &str) -> CacheEntry {
    // TODO(sourcemap): maybe log *what* we are converting?
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
    fn test_get_release_file_candidate_urls() {
        let url = "https://example.com/assets/bundle.min.js".parse().unwrap();
        let expected = &[
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(&url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz"
            .parse()
            .unwrap();
        let expected = &[
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(&url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js#wat"
            .parse()
            .unwrap();
        let expected = &[
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(&url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat"
            .parse()
            .unwrap();
        let expected = &[
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(&url).collect();
        assert_eq!(&actual, expected);
    }

    #[test]
    fn test_extract_file_stem() {
        let url = "https://example.com/bundle.js".parse().unwrap();
        assert_eq!(extract_file_stem(&url), "/bundle");

        let url = "https://example.com/bundle.min.js".parse().unwrap();
        assert_eq!(extract_file_stem(&url), "/bundle");

        let url = "https://example.com/assets/bundle.js".parse().unwrap();
        assert_eq!(extract_file_stem(&url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js".parse().unwrap();
        assert_eq!(extract_file_stem(&url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz"
            .parse()
            .unwrap();
        assert_eq!(extract_file_stem(&url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js#wat"
            .parse()
            .unwrap();
        assert_eq!(extract_file_stem(&url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat"
            .parse()
            .unwrap();
        assert_eq!(extract_file_stem(&url), "/assets/bundle");
    }

    #[test]
    fn joining_urls() {
        let base = Url::parse("https://example.com/path/to/assets/bundle.min.js?foo=1&bar=baz#wat")
            .unwrap();
        // relative
        assert_eq!(
            join_url(&base, "../sourcemaps/bundle.min.js.map")
                .unwrap()
                .as_str(),
            "https://example.com/path/to/sourcemaps/bundle.min.js.map"
        );
        // absolute
        assert_eq!(
            join_url(&base, "/foo.js").unwrap().as_str(),
            "https://example.com/foo.js"
        );
        // absolute with tilde
        assert_eq!(
            join_url(&base, "~/foo.js").unwrap().as_str(),
            "https://example.com/foo.js"
        );

        // dots
        assert_eq!(
            join_url(&base, ".././.././to/./sourcemaps/./bundle.min.js.map")
                .unwrap()
                .as_str(),
            "https://example.com/path/to/sourcemaps/bundle.min.js.map"
        );
        // unmatched dot-dots
        assert_eq!(
            join_url(&base, "../../../../../../caps-at-absolute")
                .unwrap()
                .as_str(),
            "https://example.com/caps-at-absolute"
        );
    }
}
