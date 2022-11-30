//! Service for retrieving Unity il2cpp line mapping files.
//!
//! This service downloads and caches the [`LineMapping`] used to map
//! generated C++ source files back to the original C# sources.

use std::fs::File;
use std::io::{self, Cursor, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Error};
use futures::future::{self, BoxFuture};
use sentry::{Hub, SentryFutureExt};
use tempfile::tempfile_in;

use symbolic::common::{ByteView, DebugId};
use symbolic::il2cpp::LineMapping;
use symbolicator_sources::{FileType, ObjectId, SourceConfig};

use crate::cache::{Cache, CacheStatus, ExpirationTime};
use crate::services::cacher::{CacheItemRequest, CacheKey, Cacher};
use crate::services::download::{DownloadService, DownloadStatus, RemoteDif};
use crate::types::Scope;
use crate::utils::compression::decompress_object_file;
use crate::utils::futures::{m, measure};

use super::shared_cache::SharedCacheService;

/// Handle to a valid [`LineMapping`].
///
/// While this handle points to the raw data, this data is guaranteed to be valid, you can
/// only have this handle if a positive cache existed.
#[derive(Debug, Clone)]
pub struct Il2cppHandle {
    pub debug_id: DebugId,
    pub data: ByteView<'static>,
}

impl Il2cppHandle {
    /// Parses the line mapping from the handle.
    pub fn line_mapping(&self) -> Result<LineMapping, Error> {
        LineMapping::parse(&self.data).ok_or_else(|| anyhow::anyhow!("Failed to parse LineMapping"))
    }
}

/// The handle to be returned by [`CacheItemRequest`].
///
/// This trait requires us to return a handle regardless of its cache status.
/// This is this handle but we do not expose it outside of this module, see
/// [`Il2cppHandle`] for that.
#[derive(Debug, Clone)]
struct CacheHandle {
    status: CacheStatus,
    debug_id: DebugId,
    data: ByteView<'static>,
}

/// The interface to the [`Cacher`] service.
///
/// The main work is done by the [`CacheItemRequest`] impl.
#[derive(Debug, Clone)]
struct FetchFileRequest {
    scope: Scope,
    file_source: RemoteDif,
    debug_id: DebugId,
    download_svc: Arc<DownloadService>,
    cache: Arc<Cacher<FetchFileRequest>>,
}

impl FetchFileRequest {
    /// Downloads the file and saves it to `path`.
    ///
    /// Actual implementation of [`FetchFileRequest::compute`].
    // XXX: We use a `PathBuf` here because the resulting future needs to be `'static`
    async fn fetch_file(self, path: PathBuf) -> Result<CacheStatus, Error> {
        let download_file = self.cache.tempfile()?;
        let cache_key = self.get_cache_key();

        let result = self
            .download_svc
            .download(self.file_source, download_file.path())
            .await;

        match result {
            Ok(DownloadStatus::NotFound) => {
                tracing::debug!("No il2cpp linemapping file found for {}", cache_key);
                return Ok(CacheStatus::Negative);
            }
            Err(e) => {
                let stderr: &dyn std::error::Error = &e;
                tracing::debug!(stderr, "Error while downloading file");
                return Ok(CacheStatus::CacheSpecificError(e.for_cache()));
            }
            Ok(DownloadStatus::Completed) => {
                // fall through
            }
        }
        let download_dir = download_file
            .path()
            .parent()
            .ok_or_else(|| Error::msg("Parent of download dir not found"))?;
        let decompressed_path = tempfile_in(download_dir)?;
        let mut decompressed = match decompress_object_file(&download_file, decompressed_path) {
            Ok(file) => file,
            Err(err) => {
                return Ok(CacheStatus::Malformed(err.to_string()));
            }
        };

        // Seek back to the start and parse this DIF.
        decompressed.rewind()?;
        let view = ByteView::map_file(decompressed)?;

        if LineMapping::parse(&view).is_none() {
            metric!(counter("services.il2cpp.loaderrror") += 1);
            tracing::debug!("Failed to parse il2cpp");
            return Ok(CacheStatus::Malformed("Failed to parse il2cpp".to_string()));
        }
        // The file is valid, lets save it.
        let mut destination = File::create(path)?;
        let mut cursor = Cursor::new(&view);
        io::copy(&mut cursor, &mut destination)?;

        Ok(CacheStatus::Positive)
    }
}

impl CacheItemRequest for FetchFileRequest {
    type Item = CacheHandle;
    type Error = Error;

    fn get_cache_key(&self) -> CacheKey {
        self.file_source.cache_key(self.scope.clone())
    }

    /// Downloads a file, writing it to `path`.
    ///
    /// Only when [`CacheStatus::Positive`] is returned is the data written to `path` used.
    fn compute(&self, path: &Path) -> BoxFuture<'static, Result<CacheStatus, Self::Error>> {
        let fut = self
            .clone()
            .fetch_file(path.to_path_buf())
            .bind_hub(Hub::current());

        let source_name = self.file_source.source_type_name().into();

        let future = tokio::time::timeout(Duration::from_secs(1200), fut);
        let future = measure(
            "il2cpp",
            m::timed_result,
            Some(("source_type", source_name)),
            future,
        );
        Box::pin(async move {
            future
                .await
                .map_err(|_| Error::msg("Timeout fetching il2cpp"))?
        })
    }

    fn load(
        &self,
        status: CacheStatus,
        data: ByteView<'static>,
        _expiration: ExpirationTime,
    ) -> Self::Item {
        CacheHandle {
            status,
            debug_id: self.debug_id,
            data,
        }
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

    /// Returns a `LineMapping` if one is found for the `debug_id`.
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
        let mut line_mapping_handle = None;
        for result in results {
            if result.is_some() {
                line_mapping_handle = result;
            }
        }

        let line_mapping_handle = line_mapping_handle?;

        Some(Il2cppHandle {
            debug_id: line_mapping_handle.debug_id,
            data: line_mapping_handle.data.clone(),
        })
    }

    /// Wraps `fetch_file_from_source` in sentry error handling.
    async fn fetch_file_from_source_with_error(
        &self,
        object_id: &ObjectId,
        debug_id: DebugId,
        scope: Scope,
        source: SourceConfig,
    ) -> Option<Arc<CacheHandle>> {
        let _guard = Hub::current().push_scope();
        sentry::configure_scope(|scope| {
            scope.set_tag("il2cpp.debugid", debug_id);
            scope.set_extra("il2cpp.source", source.type_name().into());
        });
        match self
            .fetch_file_from_source(object_id, debug_id, scope, source)
            .await
            .context("il2cpp svc failed for single source")
        {
            Ok(res) => res,
            Err(err) => {
                tracing::warn!("{}: {:?}", err, err.source());
                sentry::capture_error(&*err);
                None
            }
        }
    }

    /// Fetches a file and returns the [`CacheHandle`] if found.
    async fn fetch_file_from_source(
        &self,
        object_id: &ObjectId,
        debug_id: DebugId,
        scope: Scope,
        source: SourceConfig,
    ) -> Result<Option<Arc<CacheHandle>>, Error> {
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
                debug_id,
                download_svc: self.download_svc.clone(),
                cache: self.cache.clone(),
            };
            self.cache
                .compute_memoized(request)
                .bind_hub(Hub::new_from_top(Hub::current()))
        });

        let all_results = future::join_all(fetch_jobs).await;
        let mut ret = None;
        for result in all_results {
            match result {
                Ok(handle) if handle.status == CacheStatus::Positive => ret = Some(handle),
                Ok(_) => (),
                Err(err) => {
                    let stderr: &dyn std::error::Error = (*err).as_ref();
                    let mut event = sentry::event_from_error(stderr);
                    event.message = Some("Failure fetching il2cpp file from source".into());
                    sentry::capture_event(event);
                }
            }
        }
        Ok(ret)
    }
}
