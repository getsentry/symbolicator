//! Service for retrieving Unity il2cpp line mapping files.
//!
//! This service downloads and caches the [`LineMapping`] used to map
//! generated C++ source files back to the original C# sources.

use std::sync::Arc;
use std::time::Duration;

use futures::future::{self, BoxFuture};
use sentry::{Hub, SentryFutureExt};

use symbolic::common::{ByteView, DebugId};
use symbolic::il2cpp::LineMapping;
use symbolicator_sources::{FileType, ObjectId, RemoteFile, SourceConfig};
use tempfile::NamedTempFile;

use crate::caching::{
    Cache, CacheEntry, CacheError, CacheItemRequest, CacheKey, Cacher, SharedCacheRef,
};
use crate::services::download::DownloadService;
use crate::types::Scope;
use crate::utils::futures::{m, measure};

use super::fetch_file;

/// Handle to a valid [`LineMapping`].
///
/// While this handle points to the raw data, this data is guaranteed to be valid, you can
/// only have this handle if a positive cache existed.
// FIXME(swatinem): this whole type exists because the outer `FetchSymCacheInternal` type containing
// `SecondarySymCacheSources` needs to be `Clone`, and we need a `mut LineMapping` when applying it
// to a symcache, so we need to actually parse the `LineMapping` at the very end when applying it.
#[derive(Debug, Clone)]
pub struct Il2cppHandle {
    pub data: ByteView<'static>,
}

impl Il2cppHandle {
    /// Parses the line mapping from the handle.
    pub fn line_mapping(&self) -> Option<LineMapping> {
        LineMapping::parse(&self.data)
    }
}

/// The interface to the [`Cacher`] service.
///
/// The main work is done by the [`CacheItemRequest`] impl.
#[derive(Debug, Clone)]
struct FetchFileRequest {
    file_source: RemoteFile,
    download_svc: Arc<DownloadService>,
}

impl FetchFileRequest {
    /// Downloads the file and saves it to `path`.
    ///
    /// Actual implementation of [`FetchFileRequest::compute`].
    async fn fetch_file(self, temp_file: &mut NamedTempFile) -> CacheEntry {
        fetch_file(self.download_svc, self.file_source, temp_file).await?;

        let view = ByteView::map_file_ref(temp_file.as_file())?;

        if LineMapping::parse(&view).is_none() {
            metric!(counter("services.il2cpp.loaderrror") += 1);
            tracing::debug!("Failed to parse il2cpp");
            return Err(CacheError::Malformed("Failed to parse il2cpp".to_string()));
        }

        Ok(())
    }
}

impl CacheItemRequest for FetchFileRequest {
    type Item = Il2cppHandle;

    /// Downloads a file, writing it to `path`.
    ///
    /// Only when [`Ok`] is returned is the data written to `path` used.
    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        let fut = self.clone().fetch_file(temp_file).bind_hub(Hub::current());

        let timeout = Duration::from_secs(1200);
        let future = tokio::time::timeout(timeout, fut);
        let future = measure("il2cpp", m::timed_result, future);
        Box::pin(async move { future.await.map_err(|_| CacheError::Timeout(timeout))? })
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        Ok(Il2cppHandle { data })
    }
}

#[derive(Debug, Clone)]
pub struct Il2cppService {
    cache: Arc<Cacher<FetchFileRequest>>,
    download_svc: Arc<DownloadService>,
}

impl Il2cppService {
    pub fn new(
        difs_cache: Cache,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        Self {
            cache: Arc::new(Cacher::new(difs_cache, shared_cache)),
            download_svc,
        }
    }

    /// Returns an [`Il2cppHandle`] if one is found for the `debug_id`.
    pub async fn fetch_line_mapping(
        &self,
        object_id: &ObjectId,
        debug_id: DebugId,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> Option<Il2cppHandle> {
        let files = self
            .download_svc
            .list_files(&sources, &[FileType::Il2cpp], object_id)
            .await;

        let fetch_jobs = files.into_iter().map(|file_source| {
            let scope = if file_source.is_public() {
                Scope::Global
            } else {
                scope.clone()
            };
            let hub = Hub::new_from_top(Hub::current());
            hub.configure_scope(|scope| {
                scope.set_tag("il2cpp.debugid", debug_id);
                scope.set_extra("il2cpp.source", file_source.source_metric_key().into());
            });
            let cache_key = CacheKey::from_scoped_file(&scope, &file_source);
            let request = FetchFileRequest {
                file_source,
                download_svc: self.download_svc.clone(),
            };
            self.cache.compute_memoized(request, cache_key)
        });

        let all_results = future::join_all(fetch_jobs).await;
        let mut mapping = None;
        for result in all_results {
            match result {
                Ok(handle) => mapping = Some(handle),
                Err(CacheError::NotFound) => (),
                Err(error) => {
                    let error: &dyn std::error::Error = &error;
                    tracing::error!(error, "failed fetching il2cpp file");
                }
            }
        }
        mapping
    }
}
