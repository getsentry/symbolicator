//! Cache of CAB (MSZIP) envelopes synthesized on demand from decompressed objects.
//!
//! When the proxy receives a request for a Microsoft-style compressed filename
//! (`.pd_`, `.dl_`, `.ex_`) and the `raw_compressed` mirror has no upstream copy, the
//! proxy falls back to this cache, which wraps the cached decompressed object in a
//! single-folder, single-file CAB using the `cab` crate's MSZIP writer.

use std::fmt::Write as _;
use std::io::{Cursor, Seek, Write};
use std::sync::Arc;

use cab::{CabinetBuilder, CompressionType};
use futures::future::BoxFuture;
use symbolic::common::ByteView;
use symbolicator_sources::{ObjectId, RemoteFile};
use tempfile::NamedTempFile;

use crate::caches::CacheVersions;
use crate::caches::versions::CAB_SYNTH_CACHE_VERSIONS;
use crate::caching::{CacheContents, CacheError, CacheItemRequest, CacheKey, Cacher};
use crate::download::DownloadService;
use crate::types::Scope;

use super::data_cache::FetchFileDataRequest;
use super::meta_cache::FetchFileMetaRequest;
use super::role_scoped_cache_key;

const CAB_SYNTH_DISCRIMINATOR: &str = "\nrole: cab_synth\n";

/// Builds the [`CacheKey`] for a synthesized CAB envelope wrapping the given object under
/// `inner_filename`. The filename is part of the key because it is embedded in the CAB
/// header and must invalidate the cached envelope if it changes.
pub(super) fn cab_synth_cache_key(
    scope: &Scope,
    file_source: &RemoteFile,
    inner_filename: &str,
) -> CacheKey {
    let mut builder = role_scoped_cache_key(scope, file_source, CAB_SYNTH_DISCRIMINATOR);
    builder
        .write_fmt(format_args!("inner_filename: {inner_filename}\n"))
        .unwrap();
    builder.build()
}

/// Wraps `data` in a single-folder, single-file CAB (MSZIP) and writes it to `out`.
///
/// The `inner_filename` is what tools like WinDbg / `expand.exe` will see when they
/// extract the cabinet — it must be the **uncompressed** form (e.g. `foo.pdb`, never
/// `foo.pd_`).
pub fn synthesize_cab<W: Write + Seek>(
    data: &[u8],
    inner_filename: &str,
    out: W,
) -> std::io::Result<()> {
    let mut builder = CabinetBuilder::new();
    builder
        .add_folder(CompressionType::MsZip)
        .add_file(inner_filename.to_owned());

    let mut writer = builder.build(out)?;
    while let Some(mut fw) = writer.next_file()? {
        let mut src = Cursor::new(data);
        std::io::copy(&mut src, &mut fw)?;
    }
    writer.finish()?;
    Ok(())
}

/// [`CacheItemRequest`] that materializes a CAB envelope around a decompressed object.
#[derive(Clone, Debug)]
pub(crate) struct CabSynthRequest {
    pub(super) scope: Scope,
    pub(super) file_source: RemoteFile,
    pub(super) object_id: ObjectId,
    pub(super) inner_filename: String,
    pub(super) data_cache: Arc<Cacher<FetchFileDataRequest>>,
    pub(super) download_svc: Arc<DownloadService>,
}

impl CabSynthRequest {
    async fn compute_inner(&self, temp_file: &mut NamedTempFile) -> CacheContents {
        // Fetch the underlying decompressed object exactly the way `ObjectsActor::fetch`
        // does (data-cache compute_memoized + FetchFileDataRequest).
        let data_cache_key = CacheKey::from_scoped_file(&self.scope, &self.file_source);
        let meta_request = FetchFileMetaRequest {
            scope: self.scope.clone(),
            file_source: self.file_source.clone(),
            object_id: self.object_id.clone(),
            data_cache: self.data_cache.clone(),
            download_svc: self.download_svc.clone(),
            // Don't tee raw bytes here: the data_cache is being consulted to retrieve an
            // already-cached object. If it does happen to re-download, the upstream-bytes
            // mirror was either already populated by a prior fetch or wasn't requested for
            // this object type.
            raw_compressed_cache: None,
        };
        let object_handle = self
            .data_cache
            .compute_memoized(FetchFileDataRequest(meta_request), data_cache_key)
            .await
            .into_contents()?;

        // MSZIP compression of a multi-GB PDB takes tens of seconds of CPU on one thread,
        // so run it on the blocking pool to avoid starving other tokio tasks.
        let bytes = object_handle.data().clone();
        let inner_filename = self.inner_filename.clone();
        let temp_path = temp_file.path().to_owned();
        tokio::task::spawn_blocking(move || -> CacheContents {
            let mut f = std::fs::File::create(&temp_path)?;
            synthesize_cab(bytes.as_ref(), &inner_filename, &mut f)
                .map_err(|e| CacheError::Malformed(format!("CAB synthesis failed: {e}")))?;
            f.sync_all()?;
            Ok(())
        })
        .await
        .map_err(CacheError::from_std_error)??;

        temp_file.as_file_mut().rewind()?;
        Ok(())
    }
}

impl CacheItemRequest for CabSynthRequest {
    type Item = ByteView<'static>;

    const VERSIONS: CacheVersions = CAB_SYNTH_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        Box::pin(self.compute_inner(temp_file))
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        Ok(data)
    }

    fn use_shared_cache(&self) -> bool {
        // Synthesized envelopes can be regenerated cheaply from the cached object;
        // no point burning shared-cache bandwidth on them.
        false
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::*;

    #[test]
    fn test_synthesize_cab_roundtrip() {
        let payload = b"hello world, this is fake PDB content".repeat(64);
        let mut out = Cursor::new(Vec::new());
        synthesize_cab(&payload, "foo.pdb", &mut out).unwrap();

        let cab_bytes = out.into_inner();
        assert!(cab_bytes.starts_with(b"MSCF"), "must be a CAB envelope");

        let mut cabinet = cab::Cabinet::new(Cursor::new(&cab_bytes)).unwrap();
        // Collect entries first to drop the iterator borrow before we call read_file.
        let entries: Vec<String> = cabinet
            .folder_entries()
            .flat_map(|f| f.file_entries())
            .map(|f| f.name().to_owned())
            .collect();
        assert_eq!(entries, vec!["foo.pdb".to_owned()]);

        let mut extracted = Vec::new();
        cabinet
            .read_file("foo.pdb")
            .unwrap()
            .read_to_end(&mut extracted)
            .unwrap();
        assert_eq!(extracted, payload);
    }
}
