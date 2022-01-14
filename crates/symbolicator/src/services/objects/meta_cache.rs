//! Metadata cache for the object actor.
//!
//! This implements a cache holding the metadata of object files.  It does this by
//! implementing the [`CacheItemRequest`] trait for a [`FetchFileDataRequest`] which can be
//! used with a [`Cacher`] to make a filesystem based cache.
//!
//! Object metadata must be kept for longer than the data cache itself for cache
//! consistency.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use futures::future::BoxFuture;
use symbolic::common::ByteView;
use symbolic::debuginfo::Object;

use crate::cache::CacheStatus;
use crate::services::cacher::{CacheItemRequest, CacheKey, CachePath, Cacher};
use crate::services::download::{RemoteDif, RemoteDifUri};
use crate::sources::SourceId;
use crate::types::{ObjectFeatures, ObjectId, Scope};

use super::{FetchFileDataRequest, ObjectError};

/// This requests metadata of a single file at a specific path/url.
#[derive(Clone, Debug)]
pub(super) struct FetchFileMetaRequest {
    /// The scope that the file should be stored under.
    pub(super) scope: Scope,
    /// Source-type specific attributes.
    pub(super) file_source: RemoteDif,
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
    pub(super) file_source: RemoteDif,
    pub(super) features: ObjectFeatures,
    pub(super) status: CacheStatus,
}

impl ObjectMetaHandle {
    pub fn cache_key(&self) -> CacheKey {
        self.file_source.cache_key(self.scope.clone())
    }

    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    pub fn source_id(&self) -> &SourceId {
        self.file_source.source_id()
    }

    pub fn uri(&self) -> RemoteDifUri {
        self.file_source.uri()
    }

    pub fn status(&self) -> &CacheStatus {
        &self.status
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
    /// This returns [`CacheStatus::CacheSpecificError`] if the download failed.  If the
    /// data cache is [`CacheStatus::Negative`] or [`CacheStatus::Malformed`] then the same
    /// status is returned.
    ///
    /// This is the actual implementation of [`CacheItemRequest::compute`] for
    /// [`FetchFileMetaRequest`] but outside of the trait so it can be written as async/await
    /// code.
    async fn compute_file_meta(self, path: PathBuf) -> Result<CacheStatus, ObjectError> {
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file meta for {}", cache_key);

        let data_cache = self.data_cache.clone();
        let object_handle = data_cache
            .compute_memoized(FetchFileDataRequest(self))
            .await
            .map_err(ObjectError::Caching)?;
        if object_handle.status == CacheStatus::Positive {
            if let Ok(object) = Object::parse(&object_handle.data) {
                let mut new_cache = fs::File::create(path)?;

                let meta = ObjectFeatures {
                    has_debug_info: object.has_debug_info(),
                    has_unwind_info: object.has_unwind_info(),
                    has_symbols: object.has_symbols(),
                    has_sources: object.has_sources(),
                };

                log::trace!("Persisting object meta for {}: {:?}", cache_key, meta);
                serde_json::to_writer(&mut new_cache, &meta)?;
            }
        }

        // Unlike compute for other caches, this does not convert `CacheSpecificError`s
        // into `Negative` entries. This is because `create_candidate_info` populates
        // info about the original cache entry using the meta cache's data. If this
        // were to be converted to `Negative` when the original cache is a
        // `CacheSpecificError` then the user would never see what caused a download
        // failure, as what's visible to them is sourced from the output of
        //  `create_candidate_info`.
        Ok(object_handle.status.clone())
    }
}

impl CacheItemRequest for FetchFileMetaRequest {
    type Item = ObjectMetaHandle;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        self.file_source.cache_key(self.scope.clone())
    }

    fn compute(&self, path: &Path) -> BoxFuture<'static, Result<CacheStatus, Self::Error>> {
        let future = self.clone().compute_file_meta(path.to_owned());
        Box::pin(future)
    }

    fn should_load(&self, data: &[u8]) -> bool {
        serde_json::from_slice::<ObjectFeatures>(data).is_ok()
    }

    /// Returns the [`ObjectMetaHandle`] at the given cache key.
    ///
    /// If the `status` is [`CacheStatus::Malformed`] or [`CacheStatus::Negative`] the metadata
    /// returned will contain the default [`ObjectMetaHandle::features`].
    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        // When CacheStatus::Negative we get called with an empty ByteView, for Malformed we
        // get the malformed marker.
        let features = match status {
            CacheStatus::Positive => serde_json::from_slice(&data)
                .context("Failed to load positive ObjectFileMeta cache")
                .unwrap_or_else(|err| {
                    sentry::configure_scope(|scope| {
                        scope.set_extra("cache_key", self.get_cache_key().to_string().into());
                    });
                    sentry::capture_error(&*err);
                    Default::default()
                }),
            _ => Default::default(),
        };

        ObjectMetaHandle {
            scope,
            object_id: self.object_id.clone(),
            file_source: self.file_source.clone(),
            features,
            status,
        }
    }
}
