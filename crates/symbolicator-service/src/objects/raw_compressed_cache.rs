//! Cache mirror of upstream-compressed object bytes.
//!
//! When `compressed_proxy` is enabled and a downloaded object turns out to be compressed
//! (CAB/gzip/zstd/zlib/zip), the data cache tee's the still-compressed payload into a
//! sibling tempfile and persists it here via [`Cacher::store_externally`]. The `/proxy`
//! endpoint can then serve `.pd_`/`.dl_`/`.ex_` requests byte-identically by looking up
//! this cache before falling back to synthesizing a fresh CAB.
//!
//! [`Cacher::store_externally`]: crate::caching::Cacher::store_externally

use futures::future::BoxFuture;
use symbolic::common::ByteView;
use symbolicator_sources::RemoteFile;

use crate::caches::CacheVersions;
use crate::caches::versions::RAW_COMPRESSED_CACHE_VERSIONS;
use crate::caching::{CacheContents, CacheError, CacheItemRequest, CacheKey};
use crate::types::Scope;

use super::role_scoped_cache_key;

const RAW_COMPRESSED_DISCRIMINATOR: &str = "\nrole: raw_compressed\n";

/// Builds the [`CacheKey`] for the raw-compressed mirror of the given `(scope, file_source)`.
pub(super) fn raw_compressed_cache_key(scope: &Scope, file_source: &RemoteFile) -> CacheKey {
    role_scoped_cache_key(scope, file_source, RAW_COMPRESSED_DISCRIMINATOR).build()
}

/// [`CacheItemRequest`] for the raw-compressed mirror.
///
/// Carries no state -- all cache identity lives in the [`CacheKey`] built by
/// [`raw_compressed_cache_key`]. This cache is queried only via
/// [`Cacher::lookup_only`], which never invokes `compute`; it is populated as a side
/// effect of downloading the corresponding decompressed object (see
/// [`Cacher::store_externally`]).
///
/// [`Cacher::lookup_only`]: crate::caching::Cacher::lookup_only
/// [`Cacher::store_externally`]: crate::caching::Cacher::store_externally
#[derive(Clone, Debug)]
pub(crate) struct RawCompressedRequest;

impl CacheItemRequest for RawCompressedRequest {
    type Item = ByteView<'static>;

    const VERSIONS: CacheVersions = RAW_COMPRESSED_CACHE_VERSIONS;

    fn compute<'a>(
        &'a self,
        _temp_file: &'a mut tempfile::NamedTempFile,
    ) -> BoxFuture<'a, CacheContents> {
        // Unreachable: we only access this cache via `Cacher::lookup_only`. If anyone
        // ever calls `compute_memoized` on us, returning NotFound is the right
        // defensive behaviour -- the caller's fallback handles it.
        Box::pin(async { Err(CacheError::NotFound) })
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        Ok(data)
    }

    fn use_shared_cache(&self) -> bool {
        // Raw compressed mirrors are local-only — the shared cache is reserved for the
        // primary decompressed objects.
        false
    }
}
