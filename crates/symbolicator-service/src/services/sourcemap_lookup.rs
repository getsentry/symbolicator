use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;

use data_encoding::BASE64;
use futures::future::BoxFuture;
use reqwest::Url;
use sourcemap::locate_sourcemap_reference;
use symbolic::common::{ByteView, DebugId, SelfCell};
use symbolic::debuginfo::sourcebundle::{
    SourceBundleDebugSession, SourceFileDescriptor, SourceFileType,
};
use symbolic::sourcemapcache::{SourceMapCache, SourceMapCacheWriter};
use symbolicator_sources::{
    ObjectType, SentryFileId, SentryFileType, SentryRemoteFile, SentrySourceConfig,
};
use tempfile::NamedTempFile;
use url::Position;

use crate::caching::{CacheEntry, CacheError, CacheItemRequest, CacheVersions, Cacher};
use crate::services::download::sentry::SearchArtifactResult;
use crate::services::download::DownloadService;
use crate::types::{JsStacktrace, RawObjectInfo};

use super::caches::versions::{ARTIFACT_CACHE_VERSIONS, SOURCEMAP_CACHE_VERSIONS};
use super::fetch_file;

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
    // TODO: errors that happened when processing this file
    /// A flag if have already resolved the minified and sourcemap files.
    was_fetched: bool,
    /// The base url for fetching source files.
    source_file_base: Option<Url>,
    /// The fetched minified JS file.
    // TODO: maybe this should not be public?
    pub minified_source: CacheEntry<CachedFile>,
    /// The converted SourceMap.
    // TODO: maybe this should not be public?
    pub smcache: CacheEntry<OwnedSourceMapCache>,
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
            minified_source: Err(CacheError::NotFound),
            smcache: Err(CacheError::NotFound),
        }
    }

    /// TODO: we should really maintain a list of all the errors that happened for this image?
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
    files_by_key: HashMap<FileKey, CacheEntry<CachedFile>>,

    /// The [`ArtifactFetcher`] responsible for fetching artifacts, from bundles or as individual files.
    fetcher: ArtifactFetcher,
}

impl SourceMapLookup {
    pub fn new(
        artifact_caches: Arc<Cacher<FetchArtifactCacheInternal>>,
        sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
        download_svc: Arc<DownloadService>,
        source: Arc<SentrySourceConfig>,
        modules: &[RawObjectInfo],
    ) -> Self {
        let mut modules_by_abs_path = HashMap::with_capacity(modules.len());
        for module in modules {
            if module.ty != ObjectType::SourceMap {
                // TODO: raise an error?
                continue;
            }
            let Some(code_file) = module.code_file.as_ref() else {
                // TODO: raise an error?
                continue;
            };

            let debug_id = match &module.debug_id {
                Some(id) => {
                    use std::str::FromStr;
                    // TODO: raise an error?
                    DebugId::from_str(id).ok()
                }
                None => None,
            };

            let cached_module = SourceMapModule::new(code_file, debug_id);

            modules_by_abs_path.insert(code_file.to_owned(), cached_module);
        }

        let fetcher = ArtifactFetcher::new(artifact_caches, sourcemap_caches, download_svc, source);

        Self {
            modules_by_abs_path,
            files_by_key: Default::default(),
            fetcher,
        }
    }

    /// Tries to pre-fetch some of the artifacts needed for symbolication.
    pub async fn prefetch_artifacts(&mut self, stacktraces: &[JsStacktrace]) {
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

        self.fetcher.prefetch_artifacts().await;
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
        let sourcemap_url = match &minified_source {
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
    pub async fn get_source_file(&mut self, key: FileKey) -> &CacheEntry<CachedFile> {
        if !self.files_by_key.contains_key(&key) {
            let file = self.fetcher.get_file(&key).await;
            self.files_by_key.insert(key.clone(), file);
        }
        self.files_by_key.get(&key).expect("we should have a file")
    }
}

/// A URL to a sourcemap file.
///
/// May either be a conventional URL or a data URL containing the sourcemap
/// encoded as BASE64.
#[derive(Debug, Clone)]
enum SourceMapUrl {
    Data(ByteView<'static>),
    Remote(url::Url),
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
            let bv = ByteView::from_vec(decoded);
            Ok(Self::Data(bv))
        } else {
            let url = base
                .join(url_string)
                .map_err(|_| CacheError::DownloadError("Invalid sourcemap url".to_string()))?;
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
    fn abs_path(&self) -> Option<&Url> {
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

/// This is very similar to `SourceFileDescriptor`, except that it is `'static` and includes just
/// the parts that we care about.
#[derive(Debug, Clone)]
pub struct CachedFile {
    pub contents: ByteView<'static>,
    sourcemap_url: Option<Arc<SourceMapUrl>>,
    // TODO: maybe we should add a `FileOrigin` here, as in:
    // RemoteFile(Artifact)+path_in_zip ; RemoteFile ; "embedded"
}

impl CachedFile {
    fn from_descriptor(descriptor: &SourceFileDescriptor) -> Option<Self> {
        let contents = descriptor.contents()?.as_bytes().to_vec();
        let contents = ByteView::from_vec(contents);
        let sourcemap_url = match descriptor.source_mapping_url() {
            Some(url) => {
                // TODO: error handling?
                let abs_path = descriptor
                    .url()
                    .expect("descriptor should have an `abs_path`");
                let abs_path = Url::parse(abs_path).ok()?;

                SourceMapUrl::parse_with_prefix(&abs_path, url).ok()
            }
            None => None,
        };
        Some(Self {
            contents,
            sourcemap_url: sourcemap_url.map(Arc::new),
        })
    }
}

struct ArtifactFetcher {
    source: Arc<SentrySourceConfig>,
    download_svc: Arc<DownloadService>,
    remote_artifacts: HashMap<String, SearchArtifactResult>,

    #[allow(unused)]
    artifact_caches: Arc<Cacher<FetchArtifactCacheInternal>>,
    sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,

    /// The set of all the artifact bundles that we have downloaded so far.
    artifact_bundles: Vec<ArtifactBundle>,
}

impl ArtifactFetcher {
    fn new(
        artifact_caches: Arc<Cacher<FetchArtifactCacheInternal>>,
        sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
        download_svc: Arc<DownloadService>,
        source: Arc<SentrySourceConfig>,
    ) -> Self {
        Self {
            artifact_caches,
            sourcemap_caches,
            download_svc,

            source,
            remote_artifacts: Default::default(),
            artifact_bundles: Default::default(),
        }
    }

    async fn prefetch_artifacts(&mut self) {
        // TODO: filter the API call down to only look for the files we want
        self.remote_artifacts = self.list_artifacts().await;

        // TODO: actually fetch the needed artifacts :-)
    }

    async fn list_artifacts(&self) -> HashMap<String, SearchArtifactResult> {
        self.download_svc
            .list_artifacts(self.source.clone())
            .await
            .into_iter()
            .map(|artifact| (artifact.name.clone(), artifact))
            .collect()
    }

    async fn fetch_artifact(
        &self,
        source: Arc<SentrySourceConfig>,
        file_id: SentryFileId,
    ) -> CacheEntry<ByteView<'static>> {
        let mut temp_file = NamedTempFile::new().unwrap();
        fetch_file(
            self.download_svc.clone(),
            SentryRemoteFile::new(source, file_id, SentryFileType::ReleaseArtifact).into(),
            &mut temp_file,
        )
        .await?;

        Ok(ByteView::map_file(temp_file.into_file()).unwrap())
    }

    #[allow(unused)]
    async fn fetch_artifact_cache(
        &self,
        artifact: ByteView<'static>,
    ) -> CacheEntry<ByteView<'static>> {
        // TODO: really hook this up to the `Cacher`.
        // this is currently blocked on figuring out combined cache keys that depend on both
        // `source` and `sourcemap`.
        // For the time being, this all happens in a temp file that we throw away afterwards.
        let req = FetchArtifactCacheInternal { artifact };

        let mut temp_file = self.artifact_caches.tempfile()?;
        req.compute(&mut temp_file).await?;

        let temp_bv = ByteView::map_file_ref(temp_file.as_file())?;
        req.load(temp_bv)
    }

    async fn fetch_sourcemap_cache(
        &self,
        source_artifact: ByteView<'static>,
        sourcemap_artifact: ByteView<'static>,
    ) -> CacheEntry<OwnedSourceMapCache> {
        // TODO: really hook this up to the `Cacher`.
        // this is currently blocked on figuring out combined cache keys that depend on both
        // `source` and `sourcemap`.
        // For the time being, this all happens in a temp file that we throw away afterwards.
        let req = FetchSourceMapCacheInternal {
            source_artifact,
            sourcemap_artifact,
        };

        let mut temp_file = self.sourcemap_caches.tempfile()?;
        req.compute(&mut temp_file).await?;

        let temp_bv = ByteView::map_file_ref(temp_file.as_file())?;
        req.load(temp_bv)
    }

    fn find_remote_artifact(&self, url: &Url) -> Option<&SearchArtifactResult> {
        get_release_file_candidate_urls(url)
            .find_map(|candidate| self.remote_artifacts.get(&candidate))
    }

    /// Fetches the minified file, and the corresponding [`OwnedSourceMapCache`] for the file
    /// identified by its `abs_path`, or optionally its [`DebugId`].
    async fn fetch_minified_and_sourcemap(
        &mut self,
        abs_path: Url,
        debug_id: Option<DebugId>,
    ) -> (CacheEntry<CachedFile>, CacheEntry<OwnedSourceMapCache>) {
        // First, check if we have already cached / created the `SourceMapCache`.
        let key = FileKey::MinifiedSource {
            abs_path: abs_path.clone(),
            debug_id,
        };

        // Fetch the minified file first
        let minified_source = self.get_file(&key).await;

        // Then fetch the corresponding sourcemap if we have a sourcemap reference
        let sourcemap_url = match &minified_source {
            Ok(minified_source) => minified_source.sourcemap_url.as_deref(),
            Err(_) => None,
        };
        // We have three cases here:
        let sourcemap = match sourcemap_url {
            // We have an embedded SourceMap via data URL
            Some(SourceMapUrl::Data(data)) => Ok(CachedFile {
                contents: data.clone(),
                sourcemap_url: None,
            }),
            // We do have a valid `sourceMappingURL`
            Some(SourceMapUrl::Remote(url)) => {
                let sourcemap_key = FileKey::SourceMap {
                    abs_path: Some(url.clone()),
                    debug_id,
                };
                self.get_file(&sourcemap_key).await
            }
            // We *might* have a valid `DebugId`, in which case we don’t need no URL
            None => {
                let sourcemap_key = FileKey::SourceMap {
                    abs_path: None,
                    debug_id,
                };
                self.get_file(&sourcemap_key).await
            }
        };

        // Now that we (may) have both files, we can create a `SourceMapCache` for it
        let smcache = match &minified_source {
            Ok(minified_source) => match sourcemap {
                Ok(sourcemap) => {
                    // TODO: We should track the information where these files came from,
                    // and use that as the `CacheKey` for the `sourcemap_caches`.
                    self.fetch_sourcemap_cache(minified_source.contents.clone(), sourcemap.contents)
                        .await
                }
                Err(err) => Err(err),
            },
            Err(err) => Err(err.clone()),
        };

        (minified_source, smcache)
    }

    /// Fetches an arbitrary file using its `abs_path`,
    /// or optionally its [`DebugId`] and [`SourceFileType`]
    /// (because multiple files can share one [`DebugId`]).
    pub async fn get_file(&mut self, key: &FileKey) -> CacheEntry<CachedFile> {
        // Try looking up the file in one of the artifact bundles that we know about.
        if let Some(file) = self.try_get_file_from_bundles(key) {
            return Ok(file);
        }

        // Otherwise, try to get the file from an individual artifact.
        // This is mutually exclusive with having a `DebugId`, and we only care about `abs_path` here.
        // If we have a `DebugId`, we are guaranteed to use artifact bundles, and to have found the
        // file in the check up above already.
        if let Some(file) = self.try_fetch_file_from_artifacts(key).await {
            return Ok(file);
        }

        // Otherwise: Do a (cached) API lookup for the `abs_path` + `DebugId`
        if self.remote_artifacts.is_empty() {
            // TODO: the endpoint should really support filtering files,
            // and ideally only giving us a single file that matches, but right now it just gives
            // us the whole list of artifacts
            self.remote_artifacts = self.list_artifacts().await;
        }

        // At this point, *one* of our known artifacts includes the file we are looking for.
        // So we do the whole dance yet again.
        // TODO: figure out a way to avoid that?
        if let Some(file) = self.try_get_file_from_bundles(key) {
            return Ok(file);
        }
        if let Some(file) = self.try_fetch_file_from_artifacts(key).await {
            return Ok(file);
        }

        // TODO:
        // Otherwise, fall back to scraping from the Web.

        Err(CacheError::NotFound)
    }

    fn try_get_file_from_bundles(&self, key: &FileKey) -> Option<CachedFile> {
        // If we have an `id`, we first try a lookup based on that.
        if let Some(debug_id) = key.debug_id() {
            let ty = key.as_type();
            for bundle in &self.artifact_bundles {
                let bundle = bundle.get();
                if let Ok(Some(descriptor)) = bundle.source_by_debug_id(debug_id, ty) {
                    if let Some(file) = CachedFile::from_descriptor(&descriptor) {
                        return Some(file);
                    }
                }
            }
        }

        // Otherwise, try all the candidate `abs_path` patterns in every artifact bundle.
        if let Some(abs_path) = key.abs_path() {
            for url in get_release_file_candidate_urls(abs_path) {
                for bundle in &self.artifact_bundles {
                    let bundle = bundle.get();
                    if let Ok(Some(descriptor)) = bundle.source_by_url(&url) {
                        if let Some(file) = CachedFile::from_descriptor(&descriptor) {
                            return Some(file);
                        }
                    }
                }
            }
        }

        None
    }

    async fn try_fetch_file_from_artifacts(&self, key: &FileKey) -> Option<CachedFile> {
        let abs_path = key.abs_path()?;
        let found_artifact = self.find_remote_artifact(abs_path)?;

        let artifact = self
            .fetch_artifact(self.source.clone(), found_artifact.id.clone())
            .await;

        // TODO: figure out error handling:
        let contents = artifact.ok()?;

        // Get the sourcemap reference from the artifact, either from metadata, or file contents
        let sourcemap_url = resolve_sourcemap_url(abs_path, found_artifact, &contents);

        Some(CachedFile {
            contents,
            sourcemap_url: sourcemap_url.map(Arc::new),
        })
    }
}

/// Transforms a full absolute url into 2 or 4 generalized options.
// Based on `ReleaseFile.normalize`, see:
// https://github.com/getsentry/sentry/blob/master/src/sentry/models/releasefile.py
fn get_release_file_candidate_urls(url: &Url) -> impl Iterator<Item = String> {
    let mut urls = [None, None, None, None];

    // Relative without query
    if url.query().is_some() {
        urls[0] = Some(format!(
            "~{}",
            &url[Position::BeforePath..Position::AfterPath]
        ));
    }

    // Relative without fragment
    urls[1] = Some(format!(
        "~{}",
        &url[Position::BeforePath..Position::AfterQuery]
    ));

    // Absolute without query
    if url.query().is_some() {
        urls[2] = Some(url[..Position::AfterPath].to_string());
    }

    // Absolute without fragment
    urls[3] = Some(url[..Position::AfterQuery].to_string());

    urls.into_iter().flatten()
}

// Joins together frames `abs_path` and discovered sourcemap reference.
fn resolve_sourcemap_url(
    abs_path: &Url,
    source_remote_artifact: &SearchArtifactResult,
    source_artifact: &[u8],
) -> Option<SourceMapUrl> {
    if let Some(header) = source_remote_artifact.headers.get("Sourcemap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else if let Some(header) = source_remote_artifact.headers.get("X-SourceMap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else {
        let sm_ref = locate_sourcemap_reference(source_artifact).ok()??;
        SourceMapUrl::parse_with_prefix(abs_path, sm_ref.get_url()).ok()
    }
}

#[derive(Clone, Debug)]
pub struct FetchArtifactCacheInternal {
    artifact: ByteView<'static>,
}

impl CacheItemRequest for FetchArtifactCacheInternal {
    type Item = ByteView<'static>;

    const VERSIONS: CacheVersions = ARTIFACT_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(async move {
            let artifact_buf = std::str::from_utf8(&self.artifact)
                .map_err(|e| CacheError::Malformed(e.to_string()))?;
            write_artifact_cache(temp_file.as_file_mut(), artifact_buf)
        })
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        Ok(data)
    }
}

#[derive(Clone, Debug)]
pub struct FetchSourceMapCacheInternal {
    source_artifact: ByteView<'static>,
    sourcemap_artifact: ByteView<'static>,
}

impl CacheItemRequest for FetchSourceMapCacheInternal {
    type Item = OwnedSourceMapCache;

    const VERSIONS: CacheVersions = SOURCEMAP_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(async move {
            let source_artifact_buf = std::str::from_utf8(&self.source_artifact)
                .map_err(|e| CacheError::Malformed(e.to_string()))?;
            let sourcemap_artifact_buf = std::str::from_utf8(&self.sourcemap_artifact)
                .map_err(|e| CacheError::Malformed(e.to_string()))?;
            write_sourcemap_cache(
                temp_file.as_file_mut(),
                source_artifact_buf,
                sourcemap_artifact_buf,
            )
        })
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        parse_sourcemap_cache_owned(data)
    }
}

#[tracing::instrument(skip_all)]
fn write_artifact_cache(file: &mut File, source_artifact_buf: &str) -> CacheEntry {
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
fn write_sourcemap_cache(
    file: &mut File,
    source_artifact_buf: &str,
    sourcemap_artifact_buf: &str,
) -> CacheEntry {
    // TODO: maybe log *what* we are converting?
    tracing::debug!("Converting SourceMap cache");

    let smcache_writer =
        SourceMapCacheWriter::new(source_artifact_buf, sourcemap_artifact_buf).unwrap();

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
            "~/assets/bundle.min.js",
            "https://example.com/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(&url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz"
            .parse()
            .unwrap();
        let expected = &[
            "~/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(&url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js#wat"
            .parse()
            .unwrap();
        let expected = &[
            "~/assets/bundle.min.js",
            "https://example.com/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(&url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat"
            .parse()
            .unwrap();
        let expected = &[
            "~/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(&url).collect();
        assert_eq!(&actual, expected);
    }
}
