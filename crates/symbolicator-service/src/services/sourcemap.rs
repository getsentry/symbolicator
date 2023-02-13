//! Service for retrieving SourceMap artifacts.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter};
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

use crate::caching::{
    Cache, CacheEntry, CacheError, CacheItemRequest, CacheVersions, Cacher, SharedCacheRef,
};
use crate::services::download::sentry::SearchArtifactResult;
use crate::services::download::DownloadService;

use super::fetch_file;
use super::symbolication::JsProcessingSymbolicateStacktraces;

pub type OwnedSourceMapCache = SelfCell<ByteView<'static>, SourceMapCache<'static>>;

/// The supported SourceMapCache versions.
///
/// # How to version
///
/// The initial version is `1`.
/// Whenever we want to increase the version in order to re-generate stale/broken
/// sourcemap_caches, we need to:
///
/// * increase the `current` version.
/// * prepend the `current` version to the `fallbacks`.
/// * it is also possible to skip a version, in case a broken deploy needed to
///   be reverted which left behind broken sourcemap_caches.
///
/// In case a symbolic update increased its own internal format version, bump the
/// SourceMapCache file version as described above, and update the static assertion.
const SOURCEMAP_CACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[],
};

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
        match url_string.strip_prefix(Self::DATA_PREAMBLE) {
            Some(encoded) => {
                let decoded = BASE64
                    .decode(encoded.as_bytes())
                    .map_err(|_| CacheError::Malformed("Invalid base64 sourcemap".to_string()))?;
                Ok(Self::Data(decoded))
            }
            None => {
                let url = base.join(url_string).map_err(|_| {
                    CacheError::DownloadError("Invalid sourcemap url: {s}".to_string())
                })?;
                Ok(Self::Remote(url))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SourceMapService {
    sourcemap_caches: Arc<Cacher<FetchSourceMapCacheInternal>>,
    download_svc: Arc<DownloadService>,
}

impl SourceMapService {
    pub fn new(
        cache: Cache,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        Self {
            sourcemap_caches: Arc::new(Cacher::new(cache, shared_cache)),
            download_svc,
        }
    }

    pub async fn list_artifacts(
        &self,
        source: Arc<SentrySourceConfig>,
    ) -> HashMap<String, SearchArtifactResult> {
        self.download_svc
            .list_artifacts(source)
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

    pub async fn fetch_cache(
        &self,
        source: ByteView<'static>,
        sourcemap: ByteView<'static>,
    ) -> CacheEntry<OwnedSourceMapCache> {
        // TODO: really hook this up to the `Cacher`.
        // this is currently blocked on figuring out combined cache keys that depend on both
        // `source` and `sourcemap`.
        // For the time being, this all happens in a temp file that we throw away afterwards.
        let req = FetchSourceMapCacheInternal { source, sourcemap };

        let mut temp_file = self.sourcemap_caches.tempfile()?;
        req.compute(&mut temp_file).await?;

        let temp_bv = ByteView::map_file_ref(temp_file.as_file())?;
        req.load(temp_bv)
    }

    // TODO(sourcemap): Handle 3rd party servers fetching (payload should decide about it, as it's user-configurable in the UI)
    pub async fn collect_stacktrace_artifacts(
        &self,
        request: &JsProcessingSymbolicateStacktraces,
    ) -> HashMap<String, CacheEntry<OwnedSourceMapCache>> {
        let mut unique_abs_paths = HashSet::new();
        for stacktrace in &request.stacktraces {
            for frame in &stacktrace.frames {
                unique_abs_paths.insert(frame.abs_path.clone());
            }
        }

        let release_archive = self.list_artifacts(request.source.clone()).await;

        let compute_caches = unique_abs_paths.into_iter().map(|abs_path| async {
            let cache = self
                .compute_cache(&abs_path, request.source.clone(), &release_archive)
                .await;
            (abs_path, cache)
        });

        futures::future::join_all(compute_caches)
            .await
            .into_iter()
            .collect()
    }

    async fn compute_cache(
        &self,
        abs_path: &str,
        source: Arc<SentrySourceConfig>,
        release_archive: &HashMap<String, SearchArtifactResult>,
    ) -> CacheEntry<OwnedSourceMapCache> {
        let abs_path_url = Url::parse(abs_path)
            .map_err(|_| CacheError::DownloadError(format!("Invalid url: {abs_path}")))?;

        let source_artifact = get_release_file_candidate_urls(&abs_path_url)
            .into_iter()
            .find_map(|candidate| release_archive.get(&candidate))
            .ok_or_else(|| {
                CacheError::DownloadError("Could not download source file".to_string())
            })?;
        let source_file = self
            .fetch_artifact(source.clone(), source_artifact.id.clone())
            .await
            .map_err(|_| CacheError::DownloadError("Could not download source file".to_string()))?;

        let sourcemap_url =
            resolve_sourcemap_url(&abs_path_url, source_artifact, source_file.clone())
                .ok_or_else(|| CacheError::DownloadError("Sourcemap not found".into()))?;

        let sourcemap_file = match sourcemap_url {
            SourceMapUrl::Data(decoded) => ByteView::from_vec(decoded),
            SourceMapUrl::Remote(url) => {
                let sourcemap_artifact = get_release_file_candidate_urls(&url)
                    .into_iter()
                    .find_map(|candidate| release_archive.get(&candidate))
                    .ok_or_else(|| {
                        CacheError::DownloadError("Could not download sourcemap file".to_string())
                    })?;

                self.fetch_artifact(source.clone(), sourcemap_artifact.id.clone())
                    .await
                    .map_err(|_| {
                        CacheError::DownloadError("Could not download sourcemap file".to_string())
                    })?
            }
        };

        self.fetch_cache(source_file, sourcemap_file).await
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
    source_artifact: &SearchArtifactResult,
    source_file: ByteView<'static>,
) -> Option<SourceMapUrl> {
    if let Some(header) = source_artifact.headers.get("Sourcemap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else if let Some(header) = source_artifact.headers.get("X-SourceMap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else {
        let sm_ref = locate_sourcemap_reference(source_file.as_slice()).ok()??;
        SourceMapUrl::parse_with_prefix(abs_path, sm_ref.get_url()).ok()
    }
}

#[derive(Clone, Debug)]
struct FetchSourceMapCacheInternal {
    source: ByteView<'static>,
    sourcemap: ByteView<'static>,
}

impl CacheItemRequest for FetchSourceMapCacheInternal {
    type Item = OwnedSourceMapCache;

    const VERSIONS: CacheVersions = SOURCEMAP_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(async move {
            let source_buf = std::str::from_utf8(&self.source)
                .map_err(|e| CacheError::Malformed(e.to_string()))?;
            let sourcemap_buf = std::str::from_utf8(&self.sourcemap)
                .map_err(|e| CacheError::Malformed(e.to_string()))?;
            write_sourcemap_cache(temp_file.as_file_mut(), source_buf, sourcemap_buf)
        })
    }

    fn should_load(&self, _data: &[u8]) -> bool {
        true
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

/// Computes and writes theSourceMapCache.
#[tracing::instrument(skip_all)]
fn write_sourcemap_cache(file: &mut File, source_buf: &str, sourcemap_buf: &str) -> CacheEntry {
    // TODO: maybe log *what* we are converting?
    tracing::debug!("Converting SourceMap cache");

    let smcache_writer = SourceMapCacheWriter::new(source_buf, sourcemap_buf).unwrap();

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
