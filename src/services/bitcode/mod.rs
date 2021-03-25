//! Service to retrieve Apple Bitcode Symbol Maps.
//!
//! This service downloads and caches the [`PList`] and [`BCSymbolMap`] used to un-obfuscate
//! debug symbols for obfuscated Apple bitcode builds.

use std::fs::File;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Error};
use futures::compat::Future01CompatExt;
use futures::{future, FutureExt, TryFutureExt};
use sentry::integrations::anyhow::capture_anyhow;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::{ByteView, DebugId};
use symbolic::debuginfo::bcsymbolmap::BCSymbolMap;
use tempfile::tempfile_in;

use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::services::cacher::{CacheItemRequest, CachePath, Cacher};
use crate::services::download::{DownloadService, DownloadStatus, ObjectFileSource};
use crate::sources::{FileType, SourceConfig};
use crate::types::Scope;
use crate::utils::compression::decompress_object_file;
use crate::utils::futures::BoxedFuture;

mod plist;

use plist::PList;

// TODO(flub): Check all these are actually needed.
/// Handle to a valid BCSymbolMap.
///
/// While this handle points to the raw data, this data is guaranteed to be valid, you can
/// only have this handle if a positive cache existed.
#[derive(Debug, Clone)]
pub struct BCSymbolMapHandle {
    pub uuid: DebugId,
    pub source: ObjectFileSource,
    pub cache_key: CacheKey,
    pub data: ByteView<'static>,
}

impl BCSymbolMapHandle {
    /// Parses the map from the handle.
    pub fn bc_symbol_map(&self) -> Result<BCSymbolMap<'_>, Error> {
        BCSymbolMap::parse(self.uuid, &self.data).context("Failed to parse BCSymbolMap")
    }
}

/// The handle to be returned by [`CacheItemRequest`].
///
/// This trait requires us to return a handle regardless of positive, negative or malformed
/// cache status.  This is this handle but we do not expose it outside of this module, see
/// [`BCSymbolMapHandle`] for that.
#[derive(Debug, Clone)]
struct CacheHandle {
    status: CacheStatus,
    uuid: DebugId,
    source: ObjectFileSource,
    cache_key: CacheKey,
    data: ByteView<'static>,
}

/// The interface to the [`Cacher`] service.
///
/// The main work is done by the [`CacheItemRequest`] impl.
#[derive(Debug, Clone)]
struct FetchFileRequest {
    scope: Scope,
    file_source: ObjectFileSource,
    uuid: DebugId,
    download_svc: Arc<DownloadService>,
    cache: Arc<Cacher<FetchFileRequest>>,
}

impl FetchFileRequest {
    /// Downloads the file and saves it to `path`.
    ///
    /// Actual implementation of [`FetchFileRequest::compute`].
    async fn fetch_file(self, path: PathBuf) -> Result<CacheStatus, Error> {
        let download_file = self.cache.tempfile()?;
        let cache_key = self.get_cache_key();

        match self
            .download_svc
            .download(self.file_source, download_file.path().to_path_buf())
            .await?
        {
            DownloadStatus::NotFound => {
                log::debug!("No auxiliary DIF file found for {}", cache_key);
                Ok(CacheStatus::Negative)
            }
            DownloadStatus::Completed => {
                let download_dir = download_file
                    .path()
                    .parent()
                    .ok_or_else(|| Error::msg("Parent of download dir not found"))?;
                let decompressed_path = tempfile_in(download_dir)?;
                let mut decompressed =
                    match decompress_object_file(&download_file, decompressed_path) {
                        Ok(file) => file,
                        Err(_) => {
                            return Ok(CacheStatus::Malformed);
                        }
                    };

                // Seek back to the start and parse this DIF.
                decompressed.seek(SeekFrom::Start(0))?;
                let view = ByteView::map_file(decompressed)?;

                if PList::test(&view) {
                    let plist = match PList::parse(self.uuid, &view) {
                        Ok(plist) => plist,
                        Err(err) => {
                            log::debug!("Failed to parse plist: {}", err);
                            return Ok(CacheStatus::Malformed);
                        }
                    };
                    if !plist.is_bcsymbol_mapping() {
                        return Ok(CacheStatus::Malformed);
                    }
                } else if BCSymbolMap::test(&view) {
                    if let Err(err) = BCSymbolMap::parse(self.uuid, &view) {
                        log::debug!("Failed to parse bcsymbolmap: {}", err);
                        return Ok(CacheStatus::Malformed);
                    }
                } else {
                    log::debug!("Unknown file type");
                    return Ok(CacheStatus::Malformed);
                };

                // The file is valid, lets save it.
                let mut destination = File::create(path)?;
                let mut cursor = Cursor::new(&view);
                io::copy(&mut cursor, &mut destination)?;

                Ok(CacheStatus::Positive)
            }
        }
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
    fn compute(&self, path: &Path) -> BoxedFuture<Result<CacheStatus, Self::Error>> {
        let fut = self.clone().fetch_file(path.to_path_buf());
        let fut = fut
            .map_err(|e| {
                capture_anyhow(&e);
                e
            })
            .bind_hub(Hub::current())
            .boxed_local();
        let source_name = self.file_source.source_type_name();
        Box::pin(
            future_metrics!(
                "auxdifs",
                Some((Duration::from_secs(600),Error::msg("Timeout fetching aux DIF"))),
                fut.compat(),
                "source_type" => source_name,
            )
            .compat(),
        )
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
            uuid: self.uuid,
            source: self.file_source.clone(),
            cache_key: self.get_cache_key(),
            data,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BitcodeService {
    cache: Arc<Cacher<FetchFileRequest>>,
    download_svc: Arc<DownloadService>,
}

impl BitcodeService {
    pub fn new(difs_cache: Cache, download_svc: Arc<DownloadService>) -> Self {
        Self {
            cache: Arc::new(Cacher::new(difs_cache)),
            download_svc,
        }
    }

    /// Returns a `BCSymbolMap` if one is found for the UUID.
    pub async fn fetch_bcsymbolmap(
        &self,
        uuid: DebugId,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> Result<Option<BCSymbolMapHandle>, Error> {
        // First find the PList.
        let find_plist = self
            .fetch_file_from_all_sources(uuid, &[FileType::PList], scope.clone(), sources.clone())
            .await?;
        let plist_handle = match find_plist {
            Some(handle) => handle,
            None => return Ok(None),
        };

        let plist = PList::parse(uuid, &plist_handle.data)?;
        let original_uuid = match plist.original_uuid() {
            Ok(Some(uuid)) => uuid,
            _ => {
                // This should not be possible, they are written as CacheStatus::Malformed
                // for this case and we should not have found a usable PList.
                sentry::capture_message(
                    "PList did not contain valid BCSymbolMap UUID mapping",
                    sentry::Level::Error,
                );
                return Ok(None);
            }
        };

        // Next find the BCSymbolMap.
        let find_symbolmap = self
            .fetch_file_from_all_sources(original_uuid, &[FileType::BCSymbolMap], scope, sources)
            .await?;
        let symbolmap_handle = match find_symbolmap {
            Some(handle) => handle,
            None => return Ok(None),
        };

        Ok(Some(BCSymbolMapHandle {
            uuid: symbolmap_handle.uuid,
            source: symbolmap_handle.source.clone(),
            cache_key: symbolmap_handle.cache_key.clone(),
            data: symbolmap_handle.data.clone(),
        }))
    }

    async fn fetch_file_from_all_sources(
        &self,
        uuid: DebugId,
        file_type: &'static [FileType],
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> Result<Option<Arc<CacheHandle>>, Error> {
        let mut jobs = Vec::with_capacity(sources.len());
        for source in sources.iter() {
            let job = self.fetch_file_from_source(uuid, file_type, scope.clone(), source.clone());
            jobs.push(job);
        }
        let results = future::join_all(jobs).await;
        let mut ret = None;
        for result in results {
            match result {
                Ok(Some(handle)) => ret = Some(handle),
                Ok(None) => (),
                Err(err) => return Err(err),
            }
        }
        Ok(ret)
    }

    /// Fetches a file and returns the [`CacheHandle`] if found.
    ///
    /// This should on be used to fetch [`FileType::PList`] and [`FileType::BCSymbolMap`].
    async fn fetch_file_from_source(
        &self,
        uuid: DebugId,
        file_type: &'static [FileType],
        scope: Scope,
        source: SourceConfig,
    ) -> Result<Option<Arc<CacheHandle>>, Error> {
        let hub = Arc::new(Hub::new_from_top(Hub::current()));
        let file_sources = self
            .download_svc
            .clone()
            .list_files(source, file_type, uuid.into(), hub)
            .await?;

        let mut fetch_jobs = Vec::with_capacity(file_sources.len());
        for file_source in file_sources {
            let scope = if file_source.is_public() {
                Scope::Global
            } else {
                scope.clone()
            };
            let request = FetchFileRequest {
                scope,
                file_source,
                uuid,
                download_svc: self.download_svc.clone(),
                cache: self.cache.clone(),
            };
            let job = self
                .cache
                .compute_memoized(request)
                .bind_hub(sentry::Hub::new_from_top(sentry::Hub::current()));
            fetch_jobs.push(job);
        }

        let all_results = future::join_all(fetch_jobs).await;
        let mut ret = None;
        for result in all_results {
            match result {
                Ok(handle) if handle.status == CacheStatus::Positive => ret = Some(handle),
                Ok(_) => (),
                Err(err) => {
                    log::error!("Inner failure fetching auxiliary DIF file from a source");
                    capture_anyhow(&*err);
                    return Err(Error::msg("Inner failure fetching a file from a source"));
                }
            }
        }
        Ok(ret)
    }
}
