use std::fs::File;
use std::io::Cursor;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use symbolic::common::{AccessPattern, ByteView};
use symbolicator_sources::{
    HttpRemoteFile, HttpSourceConfig, SourceLocation, SymstoreIndex, SymstoreIndexBuilder,
};
use tempfile::NamedTempFile;

use crate::caches::versions::SYMSTORE_INDEX_VERSIONS;
use crate::caches::CacheVersions;
use crate::caching::{CacheContents, CacheItemRequest};

use super::http::HttpDownloader;

#[derive(Debug, Clone)]
pub struct FetchSymstoreIndex {
    pub source: Arc<HttpSourceConfig>,
    pub downloader: Arc<HttpDownloader>,
}

#[tracing::instrument(skip(downloader, source), fields(source.url = %source.url), err)]
async fn download_index_segment(
    downloader: Arc<HttpDownloader>,
    source: Arc<HttpSourceConfig>,
    segment: usize,
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
    file: &mut File,
) -> CacheContents {
    let mut lastid = Vec::new();
    let loc = "000Admin/lastid.txt";
    let remote_file = HttpRemoteFile::new(Arc::clone(&source), SourceLocation::new(loc));
    downloader
        .download_source("http", &remote_file, &mut lastid)
        .await?;

    let lastid: usize = std::str::from_utf8(&lastid).unwrap().parse().unwrap();

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
            temp_file.as_file_mut(),
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        let _result = data.hint(AccessPattern::Sequential);
        let index = SymstoreIndex::load(&data)?;
        Ok(index)
    }
}
