use std::fmt::Write;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use symbolic::common::{AccessPattern, ByteView};
use symbolicator_sources::{
    DirectoryLayoutType, FilesystemRemoteFile, FilesystemSourceConfig, GcsRemoteFile,
    GcsSourceConfig, HttpRemoteFile, HttpSourceConfig, RemoteFile, S3RemoteFile, S3SourceConfig,
    SourceConfig, SourceId, SourceIndex, SourceLocation, SymstoreIndex,
};
use tempfile::NamedTempFile;

use crate::caches::versions::SYMSTORE_INDEX_VERSIONS;
use crate::caches::CacheVersions;
use crate::caching::{
    Cache, CacheContents, CacheError, CacheItemRequest, CacheKey, Cacher, SharedCacheRef,
};
use crate::types::Scope;

use super::DownloadService;

/// The path of the file containing the ID of the most recent upload
/// log file.
const LASTID_FILE: &str = "000Admin/lastid.txt";

/// A source config for which the notion of an index makes sense.
///
/// This is essentially [`SourceConfig`] without the `Sentry` case.
/// We never want to use an index for Sentry sources.
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

/// A request to fetch a Symstore index "segment".
///
/// By "segment" we mean one of the numbered upload
/// log files that together make up the index.
#[derive(Debug, Clone)]
struct FetchSymstoreIndexSegment {
    /// Then number of the segment to fectch.
    segment_id: u32,
    /// The source for which to fetch the index.
    source: IndexSourceConfig,
    /// The download service usdd to download the
    /// segment file.
    downloader: Arc<DownloadService>,
}

/// A request to fetch an entire Symstore index.
///
/// On the source, the index exists in the form
/// of a list of numbered files, each containing
/// a subset of debug files on the source. This request
/// takes care of downloading all of these "segments"
/// and combining them into one index.
#[derive(Debug, Clone)]
struct FetchSymstoreIndex {
    /// The scope of the request.
    scope: Scope,
    /// The number of the most recently uploaded index segment.
    ///
    /// This determines how many segment files we will attempt to
    /// fetch.
    last_id: u32,
    /// A cache for index segments.
    segment_cache: Arc<Cacher<FetchSymstoreIndexSegment>>,
    /// The soure for which to fetch the index.
    source: IndexSourceConfig,
    /// The download service used to download the segment files.
    downloader: Arc<DownloadService>,
}

/// Downloads the index segment with the given number from the source, parses it,
/// and writes the result into the provided file.
#[tracing::instrument(skip(downloader, source, file), fields(source.id = %source.id()))]
async fn download_index_segment(
    downloader: Arc<DownloadService>,
    source: IndexSourceConfig,
    segment: u32,
    file: &mut File,
) -> CacheContents {
    let loc = SourceLocation::new(format!("000Admin/{segment:0>10}"));
    let remote_file = source.remote_file(loc);
    let temp_file = NamedTempFile::new()?;

    tracing::debug!(segment, "Downloading index segment");

    downloader
        .download(remote_file, temp_file.path().to_path_buf())
        .await?;

    let buf = BufReader::new(temp_file);
    let index = SymstoreIndex::parse_from_reader(buf)?;

    index.write(file)?;

    Ok(())
}

/// Downloads all the segment files up to `last_id` from the source
/// and combines them into the complete index.
///
/// The resulting index is written to the provided file.
///
/// If one segment file can't be fetched or read, the whole index
/// computation aborts. This guarantees that we don't cache incomplete
/// indexes as "succesful", but instead recompute them as soon as possible.
#[tracing::instrument(skip(cache, downloader, source, file), fields(source.id = %source.id()))]
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
                    segment = i,
                    "Failed to download symstore index segment",
                );

                return Err(CacheError::DownloadError(format!(
                    "Failed to download symstore index segment {i}"
                )));
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

/// A service for computing indexes for file sources.
///
/// In general we request every debug file from every available
/// source, but some sources provide an _index_ telling us exactly
/// which files are available on that source. If we have an index available,
/// we don't even need to make requests to the source for files not in
/// the index.
///
/// This service takes care of fetching indexes from sources which indicate
/// that they provide one (via the `has_index` field). The type of index is
/// internally determined by the source's layout, although only Symstore is
/// supported for now.
#[derive(Debug, Clone)]
pub struct SourceIndexService {
    /// The cache for storing Symstore indexes.
    symstore_cache: Arc<Cacher<FetchSymstoreIndex>>,
    /// The cache used for storing individual Symstore index segments.
    symstore_segment_cache: Arc<Cacher<FetchSymstoreIndexSegment>>,
    /// An in-memory cache for keeping track of the last segment uploaded
    /// to a Symstore index.
    symstore_last_id_cache: moka::future::Cache<(Scope, SourceId), CacheContents<u32>>,
    /// The download service to download index files.
    downloader: Arc<DownloadService>,
}

impl SourceIndexService {
    /// Creates a new `SourceIndexService`.
    ///
    /// This service will use the same cache for computing
    /// both symstore index segments and entire symstore indexes.
    pub fn new(
        cache: Cache,
        shared_cache: SharedCacheRef,
        downloader: Arc<DownloadService>,
    ) -> Self {
        // Create an in-memory cache for Symstore last IDs.
        // This is so we don't ask for the last ID on every request.
        let last_id_cache = moka::future::Cache::builder()
            .max_capacity(100)
            .name("last_id")
            .time_to_live(Duration::from_secs(24 * 60 * 60))
            .build();

        Self {
            symstore_cache: Arc::new(Cacher::new(cache.clone(), shared_cache.clone())),
            symstore_segment_cache: Arc::new(Cacher::new(cache, shared_cache)),
            symstore_last_id_cache: last_id_cache,
            downloader,
        }
    }

    /// Fetches the ID of the most recently uploaded Symstore index
    /// segment.
    ///
    /// This ID is stored in a file called `lastid.txt` in the
    /// `000Admin` directory.
    ///
    /// The last ID is locally cached for an hour, so if a new
    /// file is uploaded, we will see it at most an hour later.
    async fn fetch_symstore_last_id(
        &self,
        scope: Scope,
        source: &IndexSourceConfig,
    ) -> CacheContents<u32> {
        self.symstore_last_id_cache
            .get_with((scope, source.id().clone()), async {
                let temp_file = NamedTempFile::new()?;
                let remote_file = source.remote_file(SourceLocation::new(LASTID_FILE));
                self.downloader
                    .download(remote_file, temp_file.path().to_path_buf())
                    .await?;

                let bv = ByteView::map_file(temp_file.into_file())?;
                let last_id = std::str::from_utf8(&bv)
                    .map_err(|e| CacheError::Malformed(format!("Not valid UTF8: {e}")))?
                    .trim()
                    .parse()
                    .map_err(|e| CacheError::Malformed(format!("Not a number: {e}")))?;
                Ok(last_id)
            })
            .await
    }

    /// Fetches a Symstore index for the given source.
    async fn fetch_symstore_index(
        &self,
        scope: Scope,
        source: IndexSourceConfig,
    ) -> CacheContents<SymstoreIndex> {
        let last_id = self.fetch_symstore_last_id(scope.clone(), &source).await?;

        let request = FetchSymstoreIndex {
            scope: scope.clone(),
            last_id,
            segment_cache: self.symstore_segment_cache.clone(),
            source: source.clone(),
            downloader: self.downloader.clone(),
        };

        let mut cache_key = CacheKey::scoped_builder(&scope);
        writeln!(&mut cache_key, "type: symstore").unwrap();
        writeln!(&mut cache_key, "source_id: {}", source.id()).unwrap();
        writeln!(&mut cache_key, "last_id: {last_id}").unwrap();
        let cache_key = cache_key.build();

        self.symstore_cache
            .compute_memoized(request, cache_key)
            .await
            .into_contents()
    }

    /// Fetches a source index for the given source, if the
    /// source is configured accordingly.
    ///
    /// This returns `None` if the source does not have `has_index`
    /// set or if its layout is not supported. Currently `symstore` is
    /// the only supported layout.
    ///
    /// If fetching the index fails, an empty index is returned. This
    /// effectively disables the source because the empty index will
    /// reject any path.
    pub async fn fetch_index(&self, scope: Scope, source: &SourceConfig) -> Option<SourceIndex> {
        let source = IndexSourceConfig::maybe_from(source)?;

        if !source.has_index() {
            return None;
        }

        match source.layout_ty() {
            symbolicator_sources::DirectoryLayoutType::Symstore => {
                let source_id = source.id().clone();

                let symstore_index = match self.fetch_symstore_index(scope.clone(), source).await {
                    Ok(index) => index,
                    Err(e) => {
                        tracing::error!(
                            scope = %scope,
                            %source_id,
                            error = &e as &dyn std::error::Error,
                            "Failed to fetch Symstore index",
                        );
                        Default::default()
                    }
                };
                Some(symstore_index.into())
            }
            _ => None,
        }
    }
}
