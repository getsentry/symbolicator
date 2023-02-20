use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;

use data_encoding::BASE64;
use futures::future::BoxFuture;
use reqwest::Url;
use sourcemap::locate_sourcemap_reference;
use symbolic::common::{ByteView, SelfCell};
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

pub struct SourceMapLookup {
    source: Arc<SentrySourceConfig>,
    download_svc: Arc<DownloadService>,
    remote_artifacts: HashMap<String, SearchArtifactResult>,

    artifact_caches: Arc<Cacher<FetchArtifactCacheInternal>>,
    sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,

    artifacts: HashMap<String, CacheEntry<ByteView<'static>>>,
    sourcemaps: HashMap<String, CacheEntry<OwnedSourceMapCache>>,
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
