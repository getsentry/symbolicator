//! Service for retrieving Unity il2cpp line mapping files.
//!
//! This service downloads and caches the [`LineMapping`] used to map
//! generated C++ source files back to the original C# sources.

use std::sync::Arc;

use futures::future::{self, BoxFuture};
use sentry::{Hub, SentryFutureExt};

use symbolic::common::{ByteView, DebugId};
use symbolic::il2cpp::LineMapping;
use symbolicator_service::caches::versions::IL2CPP_CACHE_VERSIONS;
use symbolicator_service::caches::CacheVersions;
use symbolicator_service::caching::{
    Cache, CacheContents, CacheError, CacheItemRequest, CacheKey, Cacher, SharedCacheRef,
};
use symbolicator_service::download::{fetch_file, DownloadService};
use symbolicator_service::metric;
use symbolicator_service::types::Scope;
use symbolicator_sources::{FileType, ObjectId, RemoteFile, SourceConfig};
use tempfile::NamedTempFile;

/// Handle to a valid [`LineMapping`].
///
/// While this handle points to the raw data, this data is guaranteed to be valid, you can
/// only have this handle if a positive cache existed.
// FIXME(swatinem): this whole type exists because the outer `FetchSymCacheInternal` type containing
// `SecondarySymCacheSources` needs to be `Clone`, and we need a `mut LineMapping` when applying it
// to a symcache, so we need to actually parse the `LineMapping` at the very end when applying it.
#[derive(Debug, Clone)]
pub struct Il2cppHandle {
    pub file: RemoteFile,
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
    #[tracing::instrument(skip_all)]
    async fn fetch_il2cpp(&self, temp_file: &mut NamedTempFile) -> CacheContents {
        fetch_file(
            Arc::clone(&self.download_svc),
            self.file_source.clone(),
            temp_file,
        )
        .await?;

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

    const VERSIONS: CacheVersions = IL2CPP_CACHE_VERSIONS;

    /// Downloads a file, writing it to `path`.
    ///
    /// Only when [`Ok`] is returned is the data written to `path` used.
    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        Box::pin(self.fetch_il2cpp(temp_file))
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        Ok(Il2cppHandle {
            file: self.file_source.clone(),
            data,
        })
    }

    fn use_shared_cache(&self) -> bool {
        self.file_source.worth_using_shared_cache()
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
            self.cache
                .compute_memoized(request, cache_key)
                .bind_hub(hub)
        });

        let all_results = future::join_all(fetch_jobs).await;
        all_results
            .into_iter()
            .find_map(|cache_entry| cache_entry.into_contents().ok())
    }
}
