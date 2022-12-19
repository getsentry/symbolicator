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
use symbolicator_sources::{FileType, ObjectId, SourceConfig};
use tempfile::NamedTempFile;

use crate::cache::{Cache, CacheEntry, CacheError, ExpirationTime};
use crate::services::cacher::{CacheItemRequest, CacheKey, Cacher};
use crate::services::download::{DownloadService, RemoteDif};
use crate::types::Scope;
use crate::utils::futures::{m, measure};

use super::fetch_file;
use super::shared_cache::SharedCacheService;

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
    scope: Scope,
    file_source: RemoteDif,
    download_svc: Arc<DownloadService>,
}

impl FetchFileRequest {
    /// Downloads the file and saves it to `path`.
    ///
    /// Actual implementation of [`FetchFileRequest::compute`].
    async fn fetch_file(self, temp_file: NamedTempFile) -> CacheEntry<NamedTempFile> {
        let temp_file = fetch_file(self.download_svc, self.file_source, temp_file).await?;

        let view = ByteView::map_file_ref(temp_file.as_file())?;

        if LineMapping::parse(&view).is_none() {
            metric!(counter("services.il2cpp.loaderrror") += 1);
            tracing::debug!("Failed to parse il2cpp");
            return Err(CacheError::Malformed("Failed to parse il2cpp".to_string()));
        }

        Ok(temp_file)
    }
}

impl CacheItemRequest for FetchFileRequest {
    type Item = Il2cppHandle;

    fn get_cache_key(&self) -> CacheKey {
        self.file_source.cache_key(self.scope.clone())
    }

    /// Downloads a file, writing it to `path`.
    ///
    /// Only when [`Ok`] is returned is the data written to `path` used.
    fn compute(&self, temp_file: NamedTempFile) -> BoxFuture<'static, CacheEntry<NamedTempFile>> {
        let fut = self.clone().fetch_file(temp_file).bind_hub(Hub::current());

        let source_name = self.file_source.source_type_name().into();

        let timeout = Duration::from_secs(1200);
        let future = tokio::time::timeout(timeout, fut);
        let future = measure(
            "il2cpp",
            m::timed_result,
            Some(("source_type", source_name)),
            future,
        );
        Box::pin(async move { future.await.map_err(|_| CacheError::Timeout(timeout))? })
    }

    fn load(&self, data: ByteView<'static>, _expiration: ExpirationTime) -> CacheEntry<Self::Item> {
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
        shared_cache_svc: Arc<SharedCacheService>,
        download_svc: Arc<DownloadService>,
    ) -> Self {
        Self {
            cache: Arc::new(Cacher::new(difs_cache, shared_cache_svc)),
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
        let jobs = sources.iter().map(|source| {
            self.fetch_file_from_source_with_error(
                object_id,
                debug_id,
                scope.clone(),
                source.clone(),
            )
            .bind_hub(Hub::new_from_top(Hub::current()))
        });
        let results = future::join_all(jobs).await;
        let mut mapping = None;
        for result in results {
            if result.is_some() {
                mapping = result;
            }
        }
        mapping
    }

    /// Wraps `fetch_file_from_source` in sentry error handling.
    async fn fetch_file_from_source_with_error(
        &self,
        object_id: &ObjectId,
        debug_id: DebugId,
        scope: Scope,
        source: SourceConfig,
    ) -> Option<Il2cppHandle> {
        let _guard = Hub::current().push_scope();
        sentry::configure_scope(|scope| {
            scope.set_tag("il2cpp.debugid", debug_id);
            scope.set_extra("il2cpp.source", source.type_name().into());
        });
        match self.fetch_file_from_source(object_id, scope, source).await {
            Ok(mapping) => mapping,
            Err(err) => {
                let dynerr: &dyn std::error::Error = &err; // tracing expects a `&dyn Error`
                tracing::error!(error = dynerr, "failed fetching il2cpp file");
                None
            }
        }
    }

    /// Fetches a file and returns the [`Il2cppHandle`] if found.
    async fn fetch_file_from_source(
        &self,
        object_id: &ObjectId,
        scope: Scope,
        source: SourceConfig,
    ) -> CacheEntry<Option<Il2cppHandle>> {
        let file_sources = self
            .download_svc
            .list_files(source, &[FileType::Il2cpp], object_id)
            .await?;

        let fetch_jobs = file_sources.into_iter().map(|file_source| {
            let scope = if file_source.is_public() {
                Scope::Global
            } else {
                scope.clone()
            };
            let request = FetchFileRequest {
                scope,
                file_source,
                download_svc: self.download_svc.clone(),
            };
            self.cache
                .compute_memoized(request)
                .bind_hub(Hub::new_from_top(Hub::current()))
        });

        let all_results = future::join_all(fetch_jobs).await;
        let mut ret = None;
        for result in all_results {
            match result {
                Ok(handle) => ret = Some(handle),
                Err(CacheError::NotFound) => (),
                Err(err) => {
                    let dynerr: &dyn std::error::Error = &err; // tracing expects a `&dyn Error`
                    tracing::error!(error = dynerr, "failed fetching il2cpp file");
                }
            }
        }
        Ok(ret)
    }
}
