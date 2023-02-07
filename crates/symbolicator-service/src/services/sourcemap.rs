//! Service for retrieving SourceMap artifacts.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use symbolic::common::{ByteView, SelfCell};
use symbolic::sourcemapcache::{SourceMapCache, SourceMapCacheWriter};
use symbolicator_sources::{SentryFileId, SentryFileType, SentryRemoteFile, SentrySourceConfig};
use tempfile::NamedTempFile;

use crate::caching::{
    Cache, CacheEntry, CacheError, CacheItemRequest, CacheVersions, Cacher, ExpirationTime,
    SharedCacheRef,
};
use crate::services::download::sentry::SearchArtifactResult;
use crate::services::download::DownloadService;

use super::fetch_file;

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
    ) -> Option<NamedTempFile> {
        let mut temp_file = NamedTempFile::new().unwrap();
        fetch_file(
            self.download_svc.clone(),
            SentryRemoteFile::new(source, file_id, SentryFileType::ReleaseArtifact).into(),
            &mut temp_file,
        )
        .await
        .map(|_| temp_file)
        .ok()
    }

    pub async fn fetch_cache(
        &self,
        source: &NamedTempFile,
        sourcemap: &NamedTempFile,
    ) -> CacheEntry<OwnedSourceMapCache> {
        // TODO: really hook this up to the `Cacher`.
        // this is currently blocked on figuring out combined cache keys that depend on both
        // `source` and `sourcemap`.
        // For the time being, this all happens in a temp file that we throw away afterwards.
        let source = ByteView::map_file_ref(source.as_file())?;
        let sourcemap = ByteView::map_file_ref(sourcemap.as_file())?;
        let req = FetchSourceMapCacheInternal { source, sourcemap };

        let mut temp_file = self.sourcemap_caches.tempfile()?;
        req.compute(&mut temp_file).await?;

        let temp_bv = ByteView::map_file_ref(temp_file.as_file())?;
        req.load(temp_bv, ExpirationTime::TouchIn(Duration::ZERO))
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

    fn load(&self, data: ByteView<'static>, _expiration: ExpirationTime) -> CacheEntry<Self::Item> {
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
