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
use symbolicator_sources::{ObjectId, RemoteFile, RemoteFileUri, SourceId};
use tempfile::NamedTempFile;

use crate::cache::{CacheEntry, ExpirationTime};
use crate::services::cacher::{CacheItemRequest, CacheKey, Cacher};
use crate::types::{ObjectFeatures, Scope};

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
    pub(super) download_svc: Arc<crate::services::download::DownloadService>,
}

/// Handle to local metadata file of an object.
///
/// Having an instance of this type does not mean there is a downloaded object file behind
/// it. We cache metadata separately (ObjectFeatures) because every symcache lookup requires
/// reading this metadata.
#[derive(Clone, Debug)]
pub struct ObjectMetaHandle {
    pub(super) scope: Scope,
    pub(super) object_id: ObjectId,
    pub(super) file_source: RemoteFile,
    pub(super) features: ObjectFeatures,
}

impl ObjectMetaHandle {
    pub fn cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.file_source.cache_key(),
            scope: self.scope.clone(),
        }
    }

    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    pub fn source_id(&self) -> &SourceId {
        self.file_source.source_id()
    }

    pub fn uri(&self) -> RemoteFileUri {
        self.file_source.uri()
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
    /// the object metadata into the cache at `path`.  Technically the data cache could
    /// contain the object file already but this is unlikely as normally the data cache
    /// expires before the metadata cache, so if the metadata needs to be re-computed then
    /// the data cache has probably also expired.
    ///
    /// This is the actual implementation of [`CacheItemRequest::compute`] for
    /// [`FetchFileMetaRequest`] but outside of the trait so it can be written as async/await
    /// code.
    async fn compute_file_meta(&self, temp_file: &mut NamedTempFile) -> CacheEntry {
        let cache_key = self.get_cache_key();
        tracing::trace!("Fetching file meta for {}", cache_key);

        let object_handle = self
            .data_cache
            .compute_memoized(FetchFileDataRequest(self.clone()))
            .await?;

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

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.file_source.cache_key(),
            scope: self.scope.clone(),
        }
    }

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(self.compute_file_meta(temp_file))
    }

    fn should_load(&self, data: &[u8]) -> bool {
        serde_json::from_slice::<ObjectFeatures>(data).is_ok()
    }

    /// Returns the [`ObjectMetaHandle`] at the given cache key.
    fn load(&self, data: ByteView<'static>, _expiration: ExpirationTime) -> CacheEntry<Self::Item> {
        // When CacheStatus::Negative we get called with an empty ByteView, for Malformed we
        // get the malformed marker.
        let features = serde_json::from_slice(&data)?;
        Ok(Arc::new(ObjectMetaHandle {
            scope: self.scope.clone(),
            object_id: self.object_id.clone(),
            file_source: self.file_source.clone(),
            features,
        }))
    }
}
