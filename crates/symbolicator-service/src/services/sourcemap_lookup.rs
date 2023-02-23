use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;

use data_encoding::BASE64;
use futures::future::BoxFuture;
use reqwest::Url;
use sourcemap::locate_sourcemap_reference;
use symbolic::common::{AsSelf, ByteView, DebugId, SelfCell};
use symbolic::debuginfo::sourcebundle::{
    SourceBundleDebugSession, SourceFileDescriptor, SourceFileType,
};
use symbolic::sourcemapcache::{SourceMapCache, SourceMapCacheWriter};
use symbolicator_sources::{SentryFileId, SentryFileType, SentryRemoteFile, SentrySourceConfig};
use tempfile::NamedTempFile;
use url::Position;

use crate::caching::{CacheEntry, CacheError, CacheItemRequest, CacheVersions, Cacher};
use crate::services::download::sentry::SearchArtifactResult;
use crate::services::download::DownloadService;

use super::caches::versions::{ARTIFACT_CACHE_VERSIONS, SOURCEMAP_CACHE_VERSIONS};
use super::fetch_file;

pub type OwnedSourceMapCache = SelfCell<ByteView<'static>, SourceMapCache<'static>>;

/// A URL to a sourcemap file.
///
/// May either be a conventional URL or a data URL containing the sourcemap
/// encoded as BASE64.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceMapUrl {
    Data(Vec<u8>),
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
            Ok(Self::Data(decoded))
        } else {
            let url = base
                .join(url_string)
                .map_err(|_| CacheError::DownloadError("Invalid sourcemap url".to_string()))?;
            Ok(Self::Remote(url))
        }
    }
}

type ArtifactBundle = SelfCell<ByteView<'static>, SourceBundleWrapper<'static>>;

// FIXME: `SourceBundleDebugSession` should implement `AsSelf` itself :-)
struct SourceBundleWrapper<'data>(SourceBundleDebugSession<'data>);
impl<'slf> AsSelf<'slf> for SourceBundleWrapper<'_> {
    type Ref = SourceBundleDebugSession<'slf>;

    fn as_self(&'slf self) -> &Self::Ref {
        unsafe { std::mem::transmute(&self.0) }
    }
}

// FIXME: The usage of `LazyCell` inside of `SourceBundleDebugSession` makes it `!Sync`, oh well
unsafe impl Sync for SourceBundleWrapper<'_> {}

pub struct SourceMapLookup {
    source: Arc<SentrySourceConfig>,
    download_svc: Arc<DownloadService>,
    remote_artifacts: HashMap<String, SearchArtifactResult>,

    artifact_caches: Arc<Cacher<FetchArtifactCacheInternal>>,
    sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,

    artifacts: HashMap<String, CacheEntry<ByteView<'static>>>,
    sourcemaps: HashMap<String, CacheEntry<OwnedSourceMapCache>>,

    /// Arbitrary files keyed by their `abs_path`.
    files_by_path: HashMap<FileKey, CacheEntry<CachedFile>>,
    /// The set of all the artifact bundles that we have downloaded so far.
    artifact_bundles: Vec<ArtifactBundle>,
}

impl SourceMapLookup {
    pub fn new(
        artifact_caches: Arc<Cacher<FetchArtifactCacheInternal>>,
        sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
        download_svc: Arc<DownloadService>,
        source: Arc<SentrySourceConfig>,
    ) -> Self {
        Self {
            source,
            download_svc,
            remote_artifacts: HashMap::new(),

            artifact_caches,
            sourcemap_caches,

            artifacts: HashMap::new(),
            sourcemaps: HashMap::new(),

            files_by_path: HashMap::new(),
            artifact_bundles: Vec::new(),
        }
    }

    pub async fn list_artifacts(&mut self) -> HashMap<String, SearchArtifactResult> {
        self.download_svc
            .list_artifacts(self.source.clone())
            .await
            .into_iter()
            .map(|artifact| (artifact.name.clone(), artifact))
            .collect()
    }

    pub async fn fetch_artifact(
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

    pub async fn fetch_artifact_cache(
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

    pub async fn fetch_sourcemap_cache(
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

    // TODO(sourcemap): Handle 3rd party servers fetching (payload should decide about it, as it's user-configurable in the UI)
    pub async fn fetch_caches(&mut self, abs_paths: HashSet<String>) {
        self.remote_artifacts = self.list_artifacts().await;

        let compute_artifacts = abs_paths.clone().into_iter().map(|abs_path| async {
            let artifact = self.compute_artifact_cache(&abs_path).await;
            (abs_path, artifact)
        });

        self.artifacts = futures::future::join_all(compute_artifacts)
            .await
            .into_iter()
            .collect();

        let compute_sourcemaps = abs_paths.into_iter().map(|abs_path| async {
            let sourcemap = self.compute_sourcemap_cache(&abs_path).await;
            (abs_path, sourcemap)
        });

        self.sourcemaps = futures::future::join_all(compute_sourcemaps)
            .await
            .into_iter()
            .collect();
    }

    pub async fn compute_artifact_cache(&self, abs_path: &str) -> CacheEntry<ByteView<'static>> {
        let abs_path_url = Url::parse(abs_path)
            .map_err(|_| CacheError::DownloadError(format!("Invalid url: {abs_path}")))?;

        let remote_artifact = self.find_remote_artifact(&abs_path_url).ok_or_else(|| {
            CacheError::DownloadError("Could not download source file".to_string())
        })?;

        let artifact = self
            .fetch_artifact(self.source.clone(), remote_artifact.id.clone())
            .await
            .map_err(|_| CacheError::DownloadError("Could not download source file".to_string()))?;

        self.fetch_artifact_cache(artifact).await
    }

    pub async fn compute_sourcemap_cache(&self, abs_path: &str) -> CacheEntry<OwnedSourceMapCache> {
        let abs_path_url = Url::parse(abs_path)
            .map_err(|_| CacheError::DownloadError(format!("Invalid url: {abs_path}")))?;

        // TODO(sourcemap): Clean up this mess

        let source_remote_artifact = self
            .find_remote_artifact(&abs_path_url)
            .ok_or_else(|| CacheError::DownloadError("Could not download source file".to_string()));

        let source_artifact = self.lookup_artifact_cache(abs_path);

        if source_remote_artifact.is_err() || source_artifact.is_none() {
            return Err(CacheError::DownloadError("Sourcemap not found".into()));
        }

        let source_remote_artifact = source_remote_artifact.unwrap();
        let Ok(source_artifact) = source_artifact.unwrap() else {
            return Err(CacheError::DownloadError("Sourcemap not found".into()));
        };

        let sourcemap_url = resolve_sourcemap_url(
            &abs_path_url,
            source_remote_artifact,
            source_artifact.as_slice(),
        )
        .ok_or_else(|| CacheError::DownloadError("Sourcemap not found".into()))?;

        let sourcemap_artifact = match sourcemap_url {
            SourceMapUrl::Data(decoded) => ByteView::from_vec(decoded),
            SourceMapUrl::Remote(url) => {
                let sourcemap_remote_artifact =
                    self.find_remote_artifact(&url).ok_or_else(|| {
                        CacheError::DownloadError("Could not download sourcemap file".to_string())
                    })?;

                self.fetch_artifact(self.source.clone(), sourcemap_remote_artifact.id.clone())
                    .await
                    .map_err(|_| {
                        CacheError::DownloadError("Could not download sourcemap file".to_string())
                    })?
            }
        };

        self.fetch_sourcemap_cache(source_artifact.to_owned(), sourcemap_artifact)
            .await
    }

    pub fn find_remote_artifact(&self, url: &Url) -> Option<&SearchArtifactResult> {
        get_release_file_candidate_urls(url)
            .into_iter()
            .find_map(|candidate| self.remote_artifacts.get(&candidate))
    }

    pub fn lookup_artifact_cache(&self, abs_path: &str) -> Option<&CacheEntry<ByteView<'static>>> {
        self.artifacts.get(abs_path)
    }

    pub fn lookup_sourcemap_cache(
        &self,
        abs_path: &str,
    ) -> Option<&CacheEntry<OwnedSourceMapCache>> {
        self.sourcemaps.get(abs_path)
    }

    /// Fetches an arbitrary file using its `abs_path`,
    /// or optionally its [`DebugId`] and [`SourceFileType`]
    /// (because multiple files can share one [`DebugId`]).
    pub async fn get_file(&mut self, key: FileKey) -> CacheEntry<CachedFile> {
        // First, we do a trivial lookup in case we already have this file.
        // The `abs_path` uniquely identifies a file in a JS stack trace, so we use that as the
        // lookup key throughout.
        if let Some(file) = self.files_by_path.get(&key) {
            return file.clone();
        }

        // Otherwise, try looking it up in one of the artifact bundles that we already have.
        if let Some(file) = self.try_get_file_from_bundles(&key) {
            return self.files_by_path.entry(key).or_insert(Ok(file)).clone();
        }

        // Otherwise, we try to get the file from the already fetched existing individual files.

        // TODO:
        // look up every candidate `abs_path` in `self.artifacts`

        // TODO:
        // Otherwise, do a (cached) API request to Sentry and get the artifacts
        //
        //
        // NOTE: We have `self.remote_artifacts` for the API requests already.

        // TODO:
        // Otherwise, fall back to scraping from the Web.

        Err(CacheError::NotFound)
    }

    /// Gets the [`OwnedSourceMapCache`] for the file identified by its `abs_path`,
    /// or optionally its [`DebugId`].
    pub async fn get_sourcemapcache(&mut self, key: FileKey) -> CacheEntry<OwnedSourceMapCache> {
        // First, check if we have already cached / created the `SourceMapCache`.
        // if let Some(smcache) = self.sourcemaps.get(abs_path) {
        //     return smcache.clone();
        // }

        // Fetch the minified file first
        let minified_id = key
            .debug_id
            .map(|id| (id.0, SourceFileType::MinifiedSource));
        let minified_key = FileKey {
            abs_path: key.abs_path.clone(),
            debug_id: minified_id,
        };
        let minified_source = self.get_file(minified_key).await?;

        // Then fetch the corresponding sourcemap if we have a sourcemap reference
        let sourcemap_id = key.debug_id.map(|id| (id.0, SourceFileType::SourceMap));
        let sourcemap_url = match minified_source.source_mapping_url.as_deref() {
            Some(sourcemap_url) => {
                // FIXME: if we have a data URL, we don’t need the base at all
                let base_url = key.abs_path.ok_or_else(|| {
                    CacheError::DownloadError("expected minified file to have an abs_path".into())
                })?;
                Some(SourceMapUrl::parse_with_prefix(&base_url, sourcemap_url)?)
            }
            None => None,
        };

        // We have three cases here:
        let sourcemap = match sourcemap_url {
            // We have an embedded SourceMap via data URL
            Some(SourceMapUrl::Data(data)) => CachedFile {
                contents: ByteView::from_vec(data),
                source_mapping_url: None,
            },
            // We do have a valid `sourceMappingURL`
            Some(SourceMapUrl::Remote(url)) => {
                let sourcemap_key = FileKey {
                    abs_path: Some(url),
                    debug_id: sourcemap_id,
                };
                self.get_file(sourcemap_key).await?
            }
            // We *might* have a valid `DebugId`, in which case we don’t need no URL
            None => {
                let sourcemap_key = FileKey {
                    abs_path: None,
                    debug_id: sourcemap_id,
                };
                self.get_file(sourcemap_key).await?
            }
        };

        // TODO: now that we have both files, we can create a `SourceMapCache` for it
        // We should track the information where these files came from, and use that as the
        // `CacheKey` for the `sourcemap_caches`.
        drop((minified_source, sourcemap));

        Err(CacheError::NotFound)
    }

    fn try_get_file_from_bundles(&self, key: &FileKey) -> Option<CachedFile> {
        // If we have an `id`, we first try a lookup based on that.
        if let Some((debug_id, ty)) = key.debug_id {
            for bundle in &self.artifact_bundles {
                let bundle = bundle.get();
                if let Ok(Some(descriptor)) = bundle.source_by_debug_id(debug_id, ty) {
                    if let Some(file) = CachedFile::from_descriptor(&descriptor) {
                        return Some(file);
                    }
                }
            }
        }

        // TODO:
        // Otherwise, try all the candidate `abs_path` patterns in every artifact bundle.

        None
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub struct FileKey {
    abs_path: Option<Url>,
    debug_id: Option<(DebugId, SourceFileType)>,
}

impl FileKey {
    pub fn new(abs_path: &str, debug_id: Option<(DebugId, SourceFileType)>) -> CacheEntry<Self> {
        let abs_path = Url::parse(abs_path)
            .map_err(|_| CacheError::DownloadError(format!("Invalid url: {abs_path}")))?;

        Ok(Self {
            abs_path: Some(abs_path),
            debug_id,
        })
    }
}

/// This is very similar to `SourceFileDescriptor`, except that it is `'static` and includes just
/// the parts that we care about.
#[derive(Debug, Clone)]
#[allow(unused)]
pub struct CachedFile {
    contents: ByteView<'static>,
    source_mapping_url: Option<Arc<str>>,
}

impl CachedFile {
    fn from_descriptor(descriptor: &SourceFileDescriptor) -> Option<Self> {
        let contents = descriptor.contents()?.as_bytes().to_vec();
        let contents = ByteView::from_vec(contents);
        let source_mapping_url = descriptor.source_mapping_url().map(Into::into);
        Some(Self {
            contents,
            source_mapping_url,
        })
    }
}

// Transforms a full absolute url into 2 or 4 generalized options. Based on `ReleaseFile.normalize`.
// https://github.com/getsentry/sentry/blob/master/src/sentry/models/releasefile.py
fn get_release_file_candidate_urls(url: &Url) -> Vec<String> {
    let mut urls = vec![];

    // Absolute without fragment
    urls.push(url[..Position::AfterQuery].to_string());

    // Absolute without query
    if url.query().is_some() {
        urls.push(url[..Position::AfterPath].to_string())
    }

    // Relative without fragment
    urls.push(format!(
        "~{}",
        &url[Position::BeforePath..Position::AfterQuery]
    ));

    // Relative without query
    if url.query().is_some() {
        urls.push(format!(
            "~{}",
            &url[Position::BeforePath..Position::AfterPath]
        ));
    }

    urls
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
        let expected = vec![
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(&url), expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz"
            .parse()
            .unwrap();
        let expected = vec![
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(&url), expected);

        let url = "https://example.com/assets/bundle.min.js#wat"
            .parse()
            .unwrap();
        let expected = vec![
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(&url), expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat"
            .parse()
            .unwrap();
        let expected = vec![
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        assert_eq!(get_release_file_candidate_urls(&url), expected);
    }
}
