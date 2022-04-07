use std::fs::File;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Error};
use futures::future::{self, BoxFuture};
use sentry::{Hub, SentryFutureExt};
use symbolic::common::{ByteView, DebugId};
use symbolic::il2cpp::usym::UsymSymbols;
use tempfile::tempfile_in;

use crate::cache::{Cache, CacheStatus};
use crate::sources::{FileType, SourceConfig};
use crate::types::Scope;
use crate::utils::compression::decompress_object_file;
use crate::utils::futures::{m, measure};

use super::cacher::{CacheItemRequest, CacheKey, CachePath, Cacher};
use super::download::{DownloadService, DownloadStatus, RemoteDif};
use super::shared_cache::SharedCacheService;

#[derive(Debug, Clone)]
pub struct UsymHandle {
    id: DebugId,
    data: ByteView<'static>,
}

impl UsymHandle {
    /// Parses the usym from the handle.
    pub fn usym_symbols(&self) -> Result<UsymSymbols<'_>, Error> {
        UsymSymbols::parse(&self.data).context("Failed to parse Usym symbols")
    }
}

/// The handle to be returned by [`CacheItemRequest`].
///
/// This trait requires us to return a handle regardless of its cache status.
/// This is this handle but we do not expose it outside of this module, see
/// [`UsymHandle`] for that.
#[derive(Debug, Clone)]
struct CacheHandle {
    status: CacheStatus,
    id: DebugId,
    data: ByteView<'static>,
}

/// The interface to the [`Cacher`] service.
///
/// The main work is done by the [`CacheItemRequest`] impl.
#[derive(Debug, Clone)]
struct FetchFileRequest {
    scope: Scope,
    file_source: RemoteDif,
    id: DebugId,
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
                tracing::debug!("No usym file found for {}", cache_key);
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
        decompressed.seek(SeekFrom::Start(0))?;
        let view = ByteView::map_file(decompressed)?;

        if let Err(err) = UsymSymbols::parse(&view) {
            metric!(counter("services.usym.loaderrror") += 1);
            tracing::debug!("Failed to parse usym: {}", err);
            return Ok(CacheStatus::Malformed(err.to_string()));
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
            "usym",
            m::timed_result,
            Some(("source_type", source_name)),
            future,
        );
        Box::pin(async move {
            future
                .await
                .map_err(|_| Error::msg("Timeout fetching usym"))?
        })
    }

    fn load(
        &self,
        _scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _path: CachePath,
    ) -> Self::Item {
        CacheHandle {
            status,
            id: self.id,
            data,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UsymService {
    cache: Arc<Cacher<FetchFileRequest>>,
    download_svc: Arc<DownloadService>,
}

impl UsymService {
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

    /// Returns a `UsymSymbols` if one is found for the `id`.
    pub async fn fetch_usym(
        &self,
        id: DebugId,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> Option<UsymHandle> {
        let usym_handle = self.fetch_file_from_all_sources(id, scope, sources).await?;

        Some(UsymHandle {
            id: usym_handle.id,
            data: usym_handle.data.clone(),
        })
    }

    async fn fetch_file_from_all_sources(
        &self,
        id: DebugId,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> Option<Arc<CacheHandle>> {
        let jobs = sources.iter().map(|source| {
            self.fetch_file_from_source_with_error(id, scope.clone(), source.clone())
                .bind_hub(Hub::new_from_top(Hub::current()))
        });
        let results = future::join_all(jobs).await;
        let mut ret = None;
        for result in results {
            if result.is_some() {
                ret = result;
            }
        }
        ret
    }

    /// Wraps `fetch_file_from_source` in sentry error handling.
    async fn fetch_file_from_source_with_error(
        &self,
        id: DebugId,
        scope: Scope,
        source: SourceConfig,
    ) -> Option<Arc<CacheHandle>> {
        let _guard = Hub::current().push_scope();
        sentry::configure_scope(|scope| {
            scope.set_tag("usym.debugid", id);
            scope.set_extra("usym.source", source.type_name().into());
        });
        match self
            .fetch_file_from_source(id, scope, source)
            .await
            .context("Usym svc failed for single source")
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
        id: DebugId,
        scope: Scope,
        source: SourceConfig,
    ) -> Result<Option<Arc<CacheHandle>>, Error> {
        let file_sources = self
            .download_svc
            .list_files(source, &[FileType::Usym], id.into())
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
                id,
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
                    event.message = Some("Failure fetching usym file from source".into());
                    sentry::capture_event(event);
                }
            }
        }
        Ok(ret)
    }
}
