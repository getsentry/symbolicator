//! Metadata cache for the object actor.
//!
//! This implements a cache holding the metadata of object files.  It does this by
//! implementing the [`CacheItemRequest`] trait for a [`FetchFileDataRequest`] which can be
//! used with a [`Cacher`] to make a filesystem based cache.
//!
//! Object metadata must be kept for longer than the data cache itself for cache
//! consistency.

use std::sync::Arc;

use futures::future::BoxFuture;

use symbolic::common::ByteView;
use symbolicator_sources::{ObjectId, RemoteFile};
use tempfile::NamedTempFile;

use crate::caches::versions::META_CACHE_VERSIONS;
use crate::caching::{
    CacheContents, CacheItemRequest, CacheKey, CacheKeyBuilder, CacheVersions, Cacher,
};
use crate::download::DownloadService;
use crate::types::Scope;

use super::candidates::ObjectFeatures;
use super::FetchFileDataRequest;

/// This requests metadata of a single file at a specific path/url.
#[derive(Clone, Debug)]
pub(super) struct FetchFileMetaRequest {
    /// The scope that the file should be stored under.
    pub(super) scope: Scope,
    /// Source-type specific attributes.
    pub(super) file_source: RemoteFile,
    pub(super) object_id: ObjectId,

    // XXX: This kind of state is not request data. We should find a different way to get this into
    // `<FetchFileMetaRequest as CacheItemRequest>::compute`, e.g. make the Cacher hold arbitrary
    // state for computing.
    pub(super) data_cache: Arc<Cacher<FetchFileDataRequest>>,
    pub(super) download_svc: Arc<DownloadService>,
}

/// Handle to local metadata file of an object.
///
/// Having an instance of this type does not mean there is a downloaded object file behind
/// it. We cache metadata separately ([`ObjectFeatures`]) because every SymCache lookup requires
/// reading this metadata.
#[derive(Clone, Debug)]
pub struct ObjectMetaHandle {
    pub(super) scope: Scope,
    pub(super) object_id: ObjectId,
    pub(super) file_source: RemoteFile,
    pub(super) features: ObjectFeatures,
}

impl ObjectMetaHandle {
    /// Creates an [`ObjectMetaHandle`] for an arbitrary [`RemoteFile`].
    pub fn for_scoped_file(scope: Scope, file_source: RemoteFile) -> Arc<Self> {
        Arc::new(Self {
            scope,
            file_source,
            object_id: Default::default(),
            features: Default::default(),
        })
    }

    pub fn cache_key(&self) -> CacheKey {
        CacheKey::from_scoped_file(&self.scope, &self.file_source)
    }

    pub fn cache_key_builder(&self) -> CacheKeyBuilder {
        let mut builder = CacheKey::scoped_builder(&self.scope);
        builder.write_file_meta(&self.file_source).unwrap();
        builder
    }

    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn object_id(&self) -> &ObjectId {
        &self.object_id
    }
}

impl FetchFileMetaRequest {
    /// Fetches object file and derives metadata from it, storing this in the cache.
    ///
    /// This uses the data cache to fetch the requested file before parsing it and writing
    /// the object metadata into `temp_file`.  Technically the data cache could
    /// contain the object file already but this is unlikely as normally the data cache
    /// expires before the metadata cache, so if the metadata needs to be re-computed then
    /// the data cache has probably also expired.
    ///
    /// This is the actual implementation of [`CacheItemRequest::compute`] for
    /// [`FetchFileMetaRequest`] but outside of the trait so it can be written as async/await
    /// code.
    async fn compute_file_meta(&self, temp_file: &mut NamedTempFile) -> CacheContents {
        let cache_key = CacheKey::from_scoped_file(&self.scope, &self.file_source);
        tracing::trace!("Fetching file meta for {}", cache_key);

        let object_handle = self
            .data_cache
            .compute_memoized(FetchFileDataRequest(self.clone()), cache_key.clone())
            .await
            .into_contents()?;

        let object = object_handle.object();

        let meta = ObjectFeatures {
            has_debug_info: object.has_debug_info(),
            has_unwind_info: object.has_unwind_info(),
            has_symbols: object.has_symbols(),
            has_sources: object.has_sources(),
        };

        tracing::trace!("Persisting object meta for {}: {:?}", cache_key, meta);
        serde_json::to_writer(temp_file.as_file_mut(), &meta)?;

        Ok(())
    }
}

impl CacheItemRequest for FetchFileMetaRequest {
    type Item = Arc<ObjectMetaHandle>;

    const VERSIONS: CacheVersions = META_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        Box::pin(self.compute_file_meta(temp_file))
    }

    /// Returns the [`ObjectMetaHandle`] at the given cache key.
    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        let features = serde_json::from_slice(&data)?;
        Ok(Arc::new(ObjectMetaHandle {
            scope: self.scope.clone(),
            object_id: self.object_id.clone(),
            file_source: self.file_source.clone(),
            features,
        }))
    }
}
