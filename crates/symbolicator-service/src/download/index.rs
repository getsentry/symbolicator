use std::fmt::Write;
use std::fs::File;
use std::io::{BufReader, Read};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use symbolic::common::{AccessPattern, ByteView};
use symbolicator_sources::{
    DirectoryLayoutType, FilesystemRemoteFile, FilesystemSourceConfig, GcsRemoteFile,
    GcsSourceConfig, HttpRemoteFile, HttpSourceConfig, RemoteFile, S3RemoteFile, S3SourceConfig,
    SourceConfig, SourceId, SourceIndex, SourceLocation, SymstoreIndex,
};
use tempfile::NamedTempFile;

use crate::caches::versions::SYMSTORE_INDEX_VERSIONS;
use crate::caches::CacheVersions;
use crate::caching::{Cache, CacheContents, CacheItemRequest, CacheKey, Cacher, SharedCacheRef};
use crate::types::Scope;

use super::DownloadService;

const LASTID_FILE: &str = "000Admin/lastid.txt";

#[derive(Debug, Clone)]
enum IndexSourceConfig {
    Filesystem(Arc<FilesystemSourceConfig>),
    Gcs(Arc<GcsSourceConfig>),
    Http(Arc<HttpSourceConfig>),
    S3(Arc<S3SourceConfig>),
}

impl IndexSourceConfig {
    fn maybe_from(source: &SourceConfig) -> Option<Self> {
        match source {
            SourceConfig::Filesystem(fs) => Some(Self::Filesystem(Arc::clone(fs))),
            SourceConfig::Gcs(gcs) => Some(Self::Gcs(Arc::clone(gcs))),
            SourceConfig::Http(http) => Some(Self::Http(Arc::clone(http))),
            SourceConfig::S3(s3) => Some(Self::S3(Arc::clone(s3))),
            SourceConfig::Sentry(_) => None,
        }
    }

    pub fn id(&self) -> &SourceId {
        match self {
            Self::Filesystem(x) => &x.id,
            Self::Gcs(x) => &x.id,
            Self::Http(x) => &x.id,
            Self::S3(x) => &x.id,
        }
    }

    fn has_index(&self) -> bool {
        match self {
            IndexSourceConfig::Filesystem(fs) => fs.files.has_index,
            IndexSourceConfig::Gcs(gcs) => gcs.files.has_index,
            IndexSourceConfig::Http(http) => http.files.has_index,
            IndexSourceConfig::S3(s3) => s3.files.has_index,
        }
    }

    fn layout_ty(&self) -> DirectoryLayoutType {
        match self {
            IndexSourceConfig::Filesystem(fs) => fs.files.layout.ty,
            IndexSourceConfig::Gcs(gcs) => gcs.files.layout.ty,
            IndexSourceConfig::Http(http) => http.files.layout.ty,
            IndexSourceConfig::S3(s3) => s3.files.layout.ty,
        }
    }

    fn remote_file(&self, loc: SourceLocation) -> RemoteFile {
        match self {
            IndexSourceConfig::Filesystem(fs) => {
                FilesystemRemoteFile::new(Arc::clone(fs), loc).into()
            }
            IndexSourceConfig::Gcs(gcs) => GcsRemoteFile::new(Arc::clone(gcs), loc).into(),
            IndexSourceConfig::Http(http) => HttpRemoteFile::new(Arc::clone(http), loc).into(),
            IndexSourceConfig::S3(s3) => S3RemoteFile::new(Arc::clone(s3), loc).into(),
        }
    }
}

#[derive(Debug, Clone)]
struct FetchSymstoreIndexSegment {
    segment_id: u32,
    source: IndexSourceConfig,
    downloader: Arc<DownloadService>,
}

#[derive(Debug, Clone)]
struct FetchSymstoreIndex {
    scope: Scope,
    last_id: u32,
    segment_cache: Arc<Cacher<FetchSymstoreIndexSegment>>,
    source: IndexSourceConfig,
    downloader: Arc<DownloadService>,
}

#[tracing::instrument(skip(downloader, source), fields(source.id = %source.id()))]
async fn download_index_segment(
    downloader: Arc<DownloadService>,
    source: IndexSourceConfig,
    segment: u32,
    file: &mut File,
) -> CacheContents {
    let loc = SourceLocation::new(format!("000Admin/{segment:0>10}"));
    let remote_file = source.remote_file(loc);
    let temp_file = NamedTempFile::new()?;

    tracing::debug!("Downloading index segment");

    downloader
        .download(remote_file, temp_file.path().to_path_buf())
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
    source: IndexSourceConfig,
    scope: Scope,
    last_id: u32,
    file: &mut File,
) -> CacheContents {
    let mut index = SymstoreIndex::default();
    // This download is intentionally sequential. Doing it concurrently
    // causes at least the Intel symbol server to rate limit us.
    for i in 1..=last_id {
        let downloader = downloader.clone();
        let request = FetchSymstoreIndexSegment {
            segment_id: i,
            source: source.clone(),
            downloader,
        };

        let mut cache_key = CacheKey::scoped_builder(&scope);
        writeln!(&mut cache_key, "type: symstore_segment").unwrap();
        writeln!(&mut cache_key, "source_id: {}", source.id()).unwrap();
        writeln!(&mut cache_key, "segment: {i}").unwrap();
        let cache_key = cache_key.build();
        let result = cache.compute_memoized(request, cache_key).await;

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

        Box::pin(download_index_segment(
            downloader,
            self.source.clone(),
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

        Box::pin(download_full_index(
            self.segment_cache.clone(),
            downloader,
            self.source.clone(),
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
pub struct SourceIndexService {
    cache: Arc<Cacher<FetchSymstoreIndex>>,
    segment_cache: Arc<Cacher<FetchSymstoreIndexSegment>>,
    downloader: Arc<DownloadService>,
    last_id_cache: moka::future::Cache<(Scope, SourceId), CacheContents<u32>>,
}

impl SourceIndexService {
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

    async fn fetch_symstore_last_id(
        &self,
        scope: Scope,
        source: &IndexSourceConfig,
    ) -> CacheContents<u32> {
        self.last_id_cache
            .get_with((scope, source.id().clone()), async {
                let mut temp_file = NamedTempFile::new()?;
                let remote_file = source.remote_file(SourceLocation::new(LASTID_FILE));
                self.downloader
                    .download(remote_file, temp_file.path().to_path_buf())
                    .await?;

                let mut buf = String::new();
                temp_file.read_to_string(&mut buf)?;
                let last_id = std::str::from_utf8(buf.as_bytes())
                    .unwrap()
                    .parse()
                    .unwrap();
                Ok(last_id)
            })
            .await
    }

    async fn fetch_symstore_index(
        &self,
        scope: Scope,
        source: IndexSourceConfig,
    ) -> Option<SymstoreIndex> {
        let last_id = self
            .fetch_symstore_last_id(scope.clone(), &source)
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
        writeln!(&mut cache_key, "type: symstore").unwrap();
        writeln!(&mut cache_key, "source_id: {}", source.id()).unwrap();
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

    pub async fn fetch_index(&self, scope: Scope, source: &SourceConfig) -> Option<SourceIndex> {
        let source = IndexSourceConfig::maybe_from(source)?;

        if !source.has_index() {
            return None;
        }

        match source.layout_ty() {
            symbolicator_sources::DirectoryLayoutType::Symstore => self
                .fetch_symstore_index(scope, source)
                .await
                .map(SourceIndex::from),
            _ => None,
        }
    }
}
