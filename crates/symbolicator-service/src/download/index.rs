use std::fmt::Write;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use symbolic::common::{AccessPattern, ByteView};
use symbolicator_sources::{
    HttpRemoteFile, HttpSourceConfig, SourceId, SourceIndex, SourceLocation, SymstoreIndex,
};
use tempfile::NamedTempFile;

use crate::caches::versions::SYMSTORE_INDEX_VERSIONS;
use crate::caches::CacheVersions;
use crate::caching::{Cache, CacheContents, CacheItemRequest, CacheKey, Cacher, SharedCacheRef};
use crate::types::Scope;

use super::DownloadService;

#[derive(Debug, Clone)]
struct FetchSymstoreIndexSegment {
    segment_id: u32,
    source: Arc<HttpSourceConfig>,
    downloader: Arc<DownloadService>,
}

#[derive(Debug, Clone)]
struct FetchSymstoreIndex {
    scope: Scope,
    last_id: u32,
    segment_cache: Arc<Cacher<FetchSymstoreIndexSegment>>,
    source: Arc<HttpSourceConfig>,
    downloader: Arc<DownloadService>,
}

#[tracing::instrument(skip(downloader, source), fields(source.url = %source.url), err)]
async fn download_index_segment(
    downloader: Arc<DownloadService>,
    source: Arc<HttpSourceConfig>,
    segment: u32,
    file: &mut File,
) -> CacheContents {
    let loc = format!("000Admin/{segment:0>10}");
    let remote_file = HttpRemoteFile::new(Arc::clone(&source), SourceLocation::new(loc));
    let temp_file = NamedTempFile::new()?;

    tracing::debug!("Downloading index segment");

    downloader
        .download(remote_file.into(), temp_file.path().to_path_buf())
        .await?;

    let buf = BufReader::new(temp_file);
    let mut index = SymstoreIndex::default();
    index.extend_from_reader(buf)?;

    index.write(file)?;

    Ok(())
}

async fn download_full_index(
    cache: Arc<Cacher<FetchSymstoreIndexSegment>>,
    downloader: Arc<DownloadService>,
    source_config: Arc<HttpSourceConfig>,
    scope: Scope,
    last_id: u32,
    file: &mut File,
) -> CacheContents {
    let mut futures = FuturesUnordered::new();
    for i in 1..=last_id {
        let downloader = downloader.clone();
        let source = source_config.clone();
        let request = FetchSymstoreIndexSegment {
            segment_id: i,
            source,
            downloader,
        };

        let mut cache_key = CacheKey::scoped_builder(&scope);
        writeln!(&mut cache_key, "source_id: {}", source_config.id).unwrap();
        writeln!(&mut cache_key, "segment: {i}").unwrap();
        let cache_key = cache_key.build();
        futures.push(cache.compute_memoized(request, cache_key));
    }

    let mut index = SymstoreIndex::default();
    while let Some(result) = futures.next().await {
        match result.into_contents() {
            Ok(segment) => index.append(segment),
            Err(e) => {
                tracing::error!(
                    error = &e as &dyn std::error::Error,
                    "Failed to download index segment",
                );
            }
        }
    }

    index.write(file)?;

    Ok(())
}

impl CacheItemRequest for FetchSymstoreIndexSegment {
    type Item = SymstoreIndex;

    const VERSIONS: CacheVersions = SYMSTORE_INDEX_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        let downloader = Arc::clone(&self.downloader);
        let source = Arc::clone(&self.source);

        Box::pin(download_index_segment(
            downloader,
            source,
            self.segment_id,
            temp_file.as_file_mut(),
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        let _result = data.hint(AccessPattern::Sequential);
        let index = Self::Item::load(&data)?;
        Ok(index)
    }

    fn weight(item: &Self::Item) -> u32 {
        item.iter().map(|file| file.len() as u32).sum()
    }
}

impl CacheItemRequest for FetchSymstoreIndex {
    type Item = SymstoreIndex;

    const VERSIONS: CacheVersions = SYMSTORE_INDEX_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        let downloader = Arc::clone(&self.downloader);
        let source = Arc::clone(&self.source);

        Box::pin(download_full_index(
            self.segment_cache.clone(),
            downloader,
            source,
            self.scope.clone(),
            self.last_id,
            temp_file.as_file_mut(),
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        let _result = data.hint(AccessPattern::Sequential);
        let index = Self::Item::load(&data)?;
        Ok(index)
    }

    fn weight(item: &Self::Item) -> u32 {
        item.iter().map(|file| file.len() as u32).sum()
    }
}

#[derive(Debug, Clone)]
pub struct SymstoreIndexService {
    cache: Arc<Cacher<FetchSymstoreIndex>>,
    segment_cache: Arc<Cacher<FetchSymstoreIndexSegment>>,
    downloader: Arc<DownloadService>,
    last_id_cache: moka::future::Cache<(Scope, SourceId), CacheContents<u32>>,
}

impl SymstoreIndexService {
    pub fn new(
        cache: Cache,
        shared_cache: SharedCacheRef,
        downloader: Arc<DownloadService>,
    ) -> Self {
        let last_id_cache = moka::future::Cache::builder()
            .max_capacity(100)
            .name("last_id")
            .time_to_live(Duration::from_secs(24 * 60 * 60))
            .build();

        Self {
            cache: Arc::new(Cacher::new(cache.clone(), shared_cache.clone())),
            segment_cache: Arc::new(Cacher::new(cache, shared_cache)),
            downloader,
            last_id_cache,
        }
    }

    async fn get_symstore_last_id(
        &self,
        scope: Scope,
        source: Arc<HttpSourceConfig>,
    ) -> CacheContents<u32> {
        self.last_id_cache
            .get_with((scope, source.id.clone()), async {
                let temp_file = NamedTempFile::new()?;
                let loc = "000Admin/lastid.txt";
                let remote_file =
                    HttpRemoteFile::new(Arc::clone(&source), SourceLocation::new(loc));
                self.downloader
                    .download(remote_file.into(), temp_file.path().to_path_buf())
                    .await?;

                // TODO: Temporarily hardcode this to a low number for testing purposes
                // let mut buf = String::new();
                // temp_file.read_to_string(&mut buf)?;
                // let last_id = buf.parse().unwrap();
                // let last_id = std::str::from_utf8(&last_id).unwrap().parse().unwrap();
                // Ok(last_id)
                Ok(15)
            })
            .await
    }

    async fn fetch_symstore_index(
        &self,
        scope: Scope,
        source: Arc<HttpSourceConfig>,
    ) -> Option<SymstoreIndex> {
        let last_id = self
            .get_symstore_last_id(scope.clone(), source.clone())
            .await
            .ok()?;

        let request = FetchSymstoreIndex {
            scope: scope.clone(),
            last_id,
            segment_cache: self.segment_cache.clone(),
            source: source.clone(),
            downloader: self.downloader.clone(),
        };

        let mut cache_key = CacheKey::scoped_builder(&scope);
        writeln!(&mut cache_key, "source_id: {}", source.id).unwrap();
        writeln!(&mut cache_key, "last_id: {last_id}").unwrap();
        let cache_key = cache_key.build();

        let index = self
            .cache
            .compute_memoized(request, cache_key)
            .await
            .into_contents()
            .ok()?;

        Some(index)
    }

    pub async fn fetch_index(
        &self,
        scope: Scope,
        source: Arc<HttpSourceConfig>,
    ) -> Option<SourceIndex> {
        if !source.files.has_index {
            return None;
        }

        match source.files.layout.ty {
            symbolicator_sources::DirectoryLayoutType::Symstore => self
                .fetch_symstore_index(scope, source)
                .await
                .map(SourceIndex::from),
            _ => None,
        }
    }
}
