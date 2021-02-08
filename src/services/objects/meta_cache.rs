//! Metadata cache for the object actor.
//!
//! This implements a cache holding the metadata of object files.  It does this by
//! implementing the [`CacheItemRequest`] trait for a [`FetchFileDataRequest`] which can be
//! used with a [`Cacher`] to make a filesystem based cache.
//!
//! Object metadata must be kept for longer than the data cache itself for cache
//! consistency.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use symbolic::common::ByteView;
use symbolic::debuginfo::Object;

use crate::cache::{CacheKey, CacheStatus};
use crate::services::cacher::{CacheItemRequest, CachePath, Cacher};
use crate::services::download::{ObjectFileSource, ObjectFileSourceUri};
use crate::sources::SourceId;
use crate::types::{ObjectFeatures, ObjectId, Scope};
use crate::utils::futures::BoxedFuture;

use super::{FetchFileDataRequest, ObjectError, ObjectHandle};

/// This requests metadata of a single file at a specific path/url.
#[derive(Clone, Debug)]
pub(super) struct FetchFileMetaRequest {
    /// The scope that the file should be stored under.
    pub(super) scope: Scope,
    /// Source-type specific attributes.
    pub(super) file_source: ObjectFileSource,
    pub(super) object_id: ObjectId,

    // XXX: This kind of state is not request data. We should find a different way to get this into
    // `<FetchFileMetaRequest as CacheItemRequest>::compute`, e.g. make the Cacher hold arbitrary
    // state for computing.
    pub(super) data_cache: Arc<Cacher<FetchFileDataRequest>>,
    pub(super) download_svc: Arc<crate::services::download::DownloadService>,
}

/// Handle to local metadata file of an object. Having an instance of this type does not mean there
/// is a downloaded object file behind it. We cache metadata separately (ObjectFeatures) because
/// every symcache lookup requires reading this metadata.
#[derive(Clone, Debug)]
pub struct ObjectMetaHandle {
    pub(super) request: FetchFileMetaRequest,
    pub(super) scope: Scope,
    pub(super) features: ObjectFeatures,
    pub(super) status: CacheStatus,
}

impl ObjectMetaHandle {
    pub fn cache_key(&self) -> CacheKey {
        self.request.get_cache_key()
    }

    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    pub fn source_id(&self) -> &SourceId {
        self.request.file_source.source_id()
    }

    pub fn uri(&self) -> ObjectFileSourceUri {
        self.request.file_source.uri()
    }

    pub fn status(&self) -> CacheStatus {
        self.status
    }
}

impl CacheItemRequest for FetchFileMetaRequest {
    type Item = ObjectMetaHandle;
    type Error = ObjectError;

    fn get_cache_key(&self) -> CacheKey {
        CacheKey {
            cache_key: self.file_source.cache_key(),
            scope: self.scope.clone(),
        }
    }

    /// Fetches object file and derives metadata from it, storing this in the cache.
    ///
    /// This uses the data cache to fetch the requested file before parsing it and writing
    /// the object metadata into the cache at `path`.  Technically the data cache could
    /// contain the object file already but this is unlikely as normally the data cache
    /// expires before the metadata cache, so if the metadata needs to be re-computed then
    /// the data cache has probably also expired.
    ///
    /// This returns an error if the download failed.  If the data cache has a
    /// [`CacheStatus::Negative`] or [`CacheStatus::Malformed`] status the same status is
    /// returned.
    fn compute(&self, path: &Path) -> BoxedFuture<Result<CacheStatus, Self::Error>> {
        let cache_key = self.get_cache_key();
        log::trace!("Fetching file meta for {}", cache_key);

        let path = path.to_owned();
        let data_cache = self.data_cache.clone();
        let slf = self.clone();
        let result = async move {
            data_cache
                .compute_memoized(FetchFileDataRequest(slf))
                .await
                .map_err(ObjectError::Caching)
                .and_then(move |object_handle: Arc<ObjectHandle>| {
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

                    Ok(object_handle.status)
                })
        };

        Box::pin(result)
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
            CacheStatus::Positive => serde_json::from_slice(&data).unwrap_or_else(|err| {
                log::error!(
                    "Failed to load positive ObjectFileMeta cache for {:?}: {:?}",
                    self.get_cache_key(),
                    err
                );
                Default::default()
            }),
            _ => Default::default(),
        };

        ObjectMetaHandle {
            request: self.clone(),
            scope,
            features,
            status,
        }
    }
}
