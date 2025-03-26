use std::fs::File;
use std::io::Write;
use std::sync::Arc;

use futures::future::BoxFuture;
use symbolic::common::{AccessPattern, ByteView};
use symbolicator_sources::{HttpRemoteFile, HttpSourceConfig, SourceLocation, SymstoreIndex};
use tempfile::NamedTempFile;

use crate::caches::versions::SYMSTORE_INDEX_VERSIONS;
use crate::caches::CacheVersions;
use crate::caching::{CacheContents, CacheError, CacheItemRequest};

use super::http::HttpDownloader;

#[derive(Debug, Clone)]
pub struct FetchSymstoreIndex {
    pub source: Arc<HttpSourceConfig>,
    pub downloader: Arc<HttpDownloader>,
}

async fn write_symstore_index(
    downloader: Arc<HttpDownloader>,
    source: Arc<HttpSourceConfig>,
    file: &mut File,
) -> CacheContents {
    let mut index = SymstoreIndex::builder();

    let mut lastid = Vec::new();
    let loc = "000Admin/lastid.txt";
    let remote_file = HttpRemoteFile::new(Arc::clone(&source), SourceLocation::new(loc));
    downloader
        .download_source("http", &remote_file, &mut lastid)
        .await?;

    let lastid: usize = std::str::from_utf8(&lastid).unwrap().parse().unwrap();

    dbg!(lastid);

    (1..=lastid).map(|i| async {
        let loc = format!("000Admin/{i:0>10}");
        let remote_file = HttpRemoteFile::new(Arc::clone(&source), SourceLocation::new(loc));
        let mut buf = Vec::new();
        downloader
            .download_source("http", &remote_file, &mut buf)
            .await
    });

    // for i in 1.. {
    //     let loc = format!("000Admin/{i:0>10}");
    //     tracing::debug!(loc, "Downloading Symstore index file");

    //     let remote_file = HttpRemoteFile::new(Arc::clone(&source), SourceLocation::new(loc));
    //     let temp_file = NamedTempFile::new()?;
    //     let mut temp_file_tokio = tokio::fs::File::create(temp_file.path()).await?;

    //     match downloader
    //         .download_source("http", &remote_file, &mut temp_file_tokio)
    //         .await
    //     {
    //         Ok(()) => {
    //             tracing::trace!("Success");
    //         }
    //         Err(CacheError::NotFound) => {
    //             tracing::trace!("Not found");
    //             break;
    //         }
    //         Err(e) => return Err(e),
    //     }

    //     temp_file_tokio.sync_all().await?;
    //     std::mem::drop(temp_file_tokio);

    //     index.extend_from_file(temp_file.path())?;
    // }

    // let index = index.build();

    // for line in &*index.files {
    //     writeln!(file, "{line}").unwrap();
    // }
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
