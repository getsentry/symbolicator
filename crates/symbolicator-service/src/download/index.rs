use std::fmt::Write;
use std::fs::File;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use symbolic::common::{AccessPattern, ByteView};
use symbolicator_sources::{
    HttpRemoteFile, HttpSourceConfig, SourceLocation, SymstoreIndex, SymstoreIndexBuilder,
};
use tempfile::NamedTempFile;
use url::Url;

use crate::caches::versions::SYMSTORE_INDEX_VERSIONS;
use crate::caches::CacheVersions;
use crate::caching::{
    Cache, CacheContents, CacheError, CacheItemRequest, CacheKey, Cacher, SharedCacheRef,
};
use crate::types::Scope;

use super::http::HttpDownloader;

#[derive(Debug, Clone)]
pub struct FetchSymstoreIndex {
    pub lastid: u32,
    pub source: Arc<HttpSourceConfig>,
    pub downloader: Arc<HttpDownloader>,
}

#[tracing::instrument(skip(downloader, source), fields(source.url = %source.url), err)]
async fn download_index_segment(
    downloader: Arc<HttpDownloader>,
    source: Arc<HttpSourceConfig>,
    segment: u32,
) -> CacheContents<SymstoreIndexBuilder> {
    let loc = format!("000Admin/{segment:0>10}");
    let remote_file = HttpRemoteFile::new(Arc::clone(&source), SourceLocation::new(loc));
    let mut buf = Vec::new();

    tracing::debug!("Downloading index segment");

    downloader
        .download_source("http", &remote_file, &mut buf)
        .await?;

    let buf = Cursor::new(buf);
    let mut index = SymstoreIndex::builder();
    index.extend_from_reader(buf)?;

    Ok(index)
}

async fn write_symstore_index(
    downloader: Arc<HttpDownloader>,
    source: Arc<HttpSourceConfig>,
    lastid: u32,
    file: &mut File,
) -> CacheContents {
    let mut futures = FuturesUnordered::new();

    for i in 1..=lastid {
        let downloader = downloader.clone();
        let source = source.clone();
        futures.push(download_index_segment(downloader, source, i));
    }

    let mut index = SymstoreIndex::builder();
    while let Some(result) = futures.next().await {
        match result {
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

impl CacheItemRequest for FetchSymstoreIndex {
    type Item = SymstoreIndex;

    const VERSIONS: CacheVersions = SYMSTORE_INDEX_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        let downloader = Arc::clone(&self.downloader);
        let source = Arc::clone(&self.source);
        Box::pin(write_symstore_index(
            downloader,
            source,
            self.lastid,
            temp_file.as_file_mut(),
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        let _result = data.hint(AccessPattern::Sequential);
        let index = SymstoreIndex::load(&data)?;
        Ok(index)
    }
}

#[derive(Debug, Clone)]
pub(super) struct SymstoreIndexService {
    cache: Arc<Cacher<FetchSymstoreIndex>>,
    downloader: Arc<HttpDownloader>,
    lastid_cache: moka::future::Cache<Url, CacheContents<u32>>,
}

impl SymstoreIndexService {
    pub(super) fn new(
        cache: Cache,
        shared_cache: SharedCacheRef,
        downloader: Arc<HttpDownloader>,
    ) -> Self {
        let lastid_cache = moka::future::Cache::builder()
            .max_capacity(100)
            .name("lastid")
            .time_to_live(Duration::from_secs(24 * 60 * 60))
            .build();

        Self {
            cache: Arc::new(Cacher::new(cache, shared_cache)),
            downloader,
            lastid_cache,
        }
    }

    async fn get_symstore_lastid(&self, source: Arc<HttpSourceConfig>) -> CacheContents<u32> {
        self.lastid_cache
            .get_with_by_ref(&source.url, async {
                let mut lastid = Vec::new();
                let loc = "000Admin/lastid.txt";
                let remote_file =
                    HttpRemoteFile::new(Arc::clone(&source), SourceLocation::new(loc));
                self.downloader
                    .download_source("http", &remote_file, &mut lastid)
                    .await?;

                // let lastid = std::str::from_utf8(&lastid).unwrap().parse().unwrap();

                Ok(15)
            })
            .await
    }

    pub(super) async fn fetch_symstore_index(
        &self,
        scope: Scope,
        source_config: Arc<HttpSourceConfig>,
    ) -> Option<SymstoreIndex> {
        let source_id = source_config.id.to_string();
        let source_url = source_config.url.clone();

        let lastid = self.get_symstore_lastid(source_config.clone()).await.ok()?;

        let mut cache_key = CacheKey::scoped_builder(&scope);
        writeln!(&mut cache_key, "url: {}", source_config.url).unwrap();
        writeln!(&mut cache_key, "lastid: {lastid}").unwrap();
        let cache_key = cache_key.build();

        let request = FetchSymstoreIndex {
            lastid,
            source: source_config,
            downloader: Arc::clone(&self.downloader),
        };

        match self
            .cache
            .compute_memoized(request, cache_key)
            .await
            .into_contents()
        {
            Ok(index) => Some(index),
            Err(CacheError::NotFound) => None,
            Err(e) => {
                tracing::error!(
                    source_id,
                    %source_url,
                    error = &e as &dyn std::error::Error,
                    "Failed to compute symstore index",
                );
                None
            }
        }
    }
}
