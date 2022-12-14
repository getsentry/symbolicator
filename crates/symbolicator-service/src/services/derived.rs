use std::sync::Arc;

use crate::cache::{cache_entry_as_cache_status, CacheEntry, CacheError, CacheStatus};
use crate::services::objects::{FoundObject, ObjectMetaHandle};
use crate::types::{AllObjectCandidates, CandidateStatus, ObjectFeatures, ObjectUseInfo};

use super::objects::CacheLookupError;

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
    FoundObject {
        meta,
        mut candidates,
    }: FoundObject,
    candidate_status: CandidateStatus,
    derive: Derive,
) -> DerivedCache<T>
where
    T: Clone,
    Derive: FnOnce(Arc<ObjectMetaHandle>) -> Fut,
    Fut: std::future::Future<Output = CacheEntry<T>>,
{
    let meta = match meta {
        Ok(meta) => meta,
        Err(CacheLookupError { file_source, error }) => {
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
            candidates.set_status(
                candidate_status,
                file_source.source_id(),
                &file_source.uri(),
                object_info,
            );

            return DerivedCache {
                cache: Err(error),
                candidates,
                features: Default::default(),
            };
        }
    };

    // No handle => NotFound
    let Some(handle) = meta else {
        return DerivedCache {
            cache: Err(CacheError::NotFound),
            candidates,
            features: ObjectFeatures::default(),
        }
    };

    // Fetch cache file from handle
    let derived_cache = derive(Arc::clone(&handle)).await;

    candidates.set_status(
        candidate_status,
        handle.source_id(),
        &handle.uri(),
        ObjectUseInfo::from_derived_status(
            &cache_entry_as_cache_status(&derived_cache),
            // FIXME(swatinem): figure out what to do here?
            &CacheStatus::Positive,
        ),
    );

    DerivedCache {
        cache: derived_cache,
        candidates,
        features: handle.features(),
    }
}
