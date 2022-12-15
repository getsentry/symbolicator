use std::sync::Arc;

use crate::cache::{cache_entry_as_cache_status, CacheEntry, CacheError, CacheStatus};
use crate::services::objects::{FindResult, ObjectMetaHandle};
use crate::types::{AllObjectCandidates, CandidateStatus, ObjectFeatures, ObjectUseInfo};

/// This is the result of fetching a derived cache file.
///
/// This has the requested [`CacheEntry`], as well as [`AllObjectCandidates`] that were considered
/// and the [`ObjectFeatures`] of the primarily used object file.
#[derive(Clone, Debug)]
pub struct DerivedCache<T> {
    pub cache: CacheEntry<T>,
    pub candidates: AllObjectCandidates,
    pub features: ObjectFeatures,
}

/// Derives a [`DerivedCache`] from the provided object handle and derive function.
///
/// This function is mainly a wrapper that simplifies error handling and propagation of
/// [`AllObjectCandidates`] and [`ObjectFeatures`].
/// The [`CandidateStatus`] is responsible for telling which status to set on the found candidate.
pub async fn derive_from_object_handle<T, Derive, Fut>(
    FindResult {
        meta,
        mut candidates,
    }: FindResult,
    candidate_status: CandidateStatus,
    derive: Derive,
) -> DerivedCache<T>
where
    T: Clone,
    Derive: FnOnce(Arc<ObjectMetaHandle>) -> Fut,
    Fut: std::future::Future<Output = CacheEntry<T>>,
{
    let Some(meta) = meta else {
        return DerivedCache {
            cache: Err(CacheError::NotFound),
            candidates,
            features: ObjectFeatures::default(),
        };
    };

    let (cache, object_info, features) = match meta.handle {
        Ok(handle) => {
            // Fetch cache file from handle
            let derived_cache = derive(Arc::clone(&handle)).await;

            let object_info = ObjectUseInfo::from_derived_status(
                &cache_entry_as_cache_status(&derived_cache),
                &CacheStatus::Positive,
            );
            (derived_cache, object_info, handle.features())
        }
        Err(error) => {
            let object_info = match &error {
                CacheError::NotFound
                | CacheError::PermissionDenied(_)
                | CacheError::Timeout(_)
                | CacheError::DownloadError(_) => {
                    // FIXME(swatinem): all the download errors so far lead to `None`
                    // previously. we should probably decide on a better solution here.
                    ObjectUseInfo::None
                }
                error => {
                    let status = error.as_cache_status();
                    ObjectUseInfo::from_derived_status(&status, &status)
                }
            };

            (Err(error), object_info, Default::default())
        }
    };

    let source_id = meta.file_source.source_id();
    let uri = meta.file_source.uri();
    candidates.set_status(candidate_status, source_id, &uri, object_info);

    DerivedCache {
        cache,
        candidates,
        features,
    }
}
