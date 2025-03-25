use std::fs::File;
use std::io::Write;

use futures::future::BoxFuture;
use symbolic::common::{AccessPattern, ByteView};
use symbolicator_sources::SymstoreIndex;
use tempfile::NamedTempFile;
use url::Url;

use crate::caches::versions::SYMSTORE_INDEX_VERSIONS;
use crate::caches::CacheVersions;
use crate::caching::{CacheContents, CacheItemRequest};

#[derive(Debug, Clone)]
pub struct FetchSymstoreIndex {
    pub url: Url,
}

fn write_symstore_index(file: &mut File) -> CacheContents {
    let mut index = SymstoreIndex::builder();
    for i in 1..=209 {
        let path = format!("/Users/sebastian/Downloads/Intel/000Admin/{i:0>10}");
        index.extend_from_file(&path).unwrap();
    }

    let index = index.build();

    for line in &*index.files {
        writeln!(file, "{line}").unwrap();
    }
    Ok(())
}

impl CacheItemRequest for FetchSymstoreIndex {
    type Item = SymstoreIndex;

    const VERSIONS: CacheVersions = SYMSTORE_INDEX_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        Box::pin(async { write_symstore_index(temp_file.as_file_mut()) })
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        let _result = data.hint(AccessPattern::Sequential);
        let index = SymstoreIndex::load(&data)?;
        Ok(index)
    }
}
