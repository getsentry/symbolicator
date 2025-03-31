//! Service to retrieve Apple Bitcode Symbol Maps.
//!
//! This service downloads and caches the `PList` and [`BcSymbolMap`] used to un-obfuscate
//! debug symbols for obfuscated Apple bitcode builds.

use std::fmt::{self, Display};
use std::sync::Arc;

use anyhow::Context;
use futures::future::{self, BoxFuture};
use sentry::{Hub, SentryFutureExt};

use symbolic::common::{ByteView, DebugId};
use symbolic::debuginfo::macho::{BcSymbolMap, UuidMapping};
use symbolicator_service::caches::versions::BITCODE_CACHE_VERSIONS;
use symbolicator_service::caches::CacheVersions;
use symbolicator_service::caching::{
    Cache, CacheContents, CacheError, CacheItemRequest, CacheKey, Cacher, SharedCacheRef,
};
use symbolicator_service::download::{self, fetch_file, DownloadService, SymstoreIndexService};
use symbolicator_service::metric;
use symbolicator_service::types::Scope;
use symbolicator_sources::{FileType, RemoteFile, SourceConfig};
use tempfile::NamedTempFile;

/// Handle to a valid BCSymbolMap.
///
/// While this handle points to the raw data, this data is guaranteed to be valid, you can
/// only have this handle if a positive cache existed.
#[derive(Debug, Clone)]
pub struct BcSymbolMapHandle {
    pub file: RemoteFile,
    pub uuid: DebugId,
    pub data: ByteView<'static>,
}

impl BcSymbolMapHandle {
    /// Parses the map from the handle.
    pub fn bc_symbol_map(&self) -> anyhow::Result<BcSymbolMap<'_>> {
        BcSymbolMap::parse(&self.data).context("Failed to parse BCSymbolMap")
    }
}

/// The handle to be returned by [`CacheItemRequest`].
///
/// This trait requires us to return a handle regardless of its cache status.
/// This is this handle but we do not expose it outside of this module, see
/// [`BcSymbolMapHandle`] for that.
#[derive(Debug, Clone)]
struct CacheHandle {
    file: RemoteFile,
    uuid: DebugId,
    data: ByteView<'static>,
}

/// The kinds of DIFs which are stored in the cache accessed from [`FetchFileRequest`].
#[derive(Debug, Clone, Copy)]
enum AuxDifKind {
    BcSymbolMap,
    UuidMap,
}

impl Display for AuxDifKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AuxDifKind::BcSymbolMap => write!(f, "BCSymbolMap"),
            AuxDifKind::UuidMap => write!(f, "UuidMap"),
        }
    }
}

/// The interface to the [`Cacher`] service.
///
/// The main work is done by the [`CacheItemRequest`] impl.
#[derive(Debug, Clone)]
struct FetchFileRequest {
    file_source: RemoteFile,
    uuid: DebugId,
    kind: AuxDifKind,
    download_svc: Arc<DownloadService>,
}

impl FetchFileRequest {
    #[tracing::instrument(skip(self, temp_file), fields(kind = %self.kind))]
    async fn fetch_auxdif(&self, temp_file: &mut NamedTempFile) -> CacheContents {
        fetch_file(
            self.download_svc.clone(),
            self.file_source.clone(),
            temp_file,
        )
        .await?;

        let view = ByteView::map_file_ref(temp_file.as_file())?;

        match self.kind {
            AuxDifKind::BcSymbolMap => {
                if let Err(err) = BcSymbolMap::parse(&view) {
                    let kind = self.kind.to_string();
                    metric!(counter("services.bitcode.loaderrror") += 1, "kind" => &kind);
                    tracing::debug!("Failed to parse bcsymbolmap: {}", err);
                    return Err(CacheError::Malformed(err.to_string()));
                }
            }
            AuxDifKind::UuidMap => {
                if let Err(err) = UuidMapping::parse_plist(self.uuid, &view) {
                    let kind = self.kind.to_string();
                    metric!(counter("services.bitcode.loaderrror") += 1, "kind" => &kind);
                    tracing::debug!("Failed to parse plist: {}", err);
                    return Err(CacheError::Malformed(err.to_string()));
                }
            }
        }
        Ok(())
    }
}

impl CacheItemRequest for FetchFileRequest {
    type Item = Arc<CacheHandle>;

    const VERSIONS: CacheVersions = BITCODE_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheContents> {
        Box::pin(self.fetch_auxdif(temp_file))
    }

    fn load(&self, data: ByteView<'static>) -> CacheContents<Self::Item> {
        Ok(Arc::new(CacheHandle {
            file: self.file_source.clone(),
            uuid: self.uuid,
            data,
        }))
    }

    fn use_shared_cache(&self) -> bool {
        self.file_source.worth_using_shared_cache()
    }
}

#[derive(Debug, Clone)]
pub struct BitcodeService {
    cache: Arc<Cacher<FetchFileRequest>>,
    download_svc: Arc<DownloadService>,
    symstore_index_svc: Arc<SymstoreIndexService>,
}

impl BitcodeService {
    pub fn new(
        difs_cache: Cache,
        shared_cache: SharedCacheRef,
        download_svc: Arc<DownloadService>,
        symstore_index_svc: Arc<SymstoreIndexService>,
    ) -> Self {
        Self {
            cache: Arc::new(Cacher::new(difs_cache, Arc::clone(&shared_cache))),
            download_svc,
            symstore_index_svc,
        }
    }

    /// Returns a `BCSymbolMap` if one is found for the `uuid`.
    pub async fn fetch_bcsymbolmap(
        &self,
        uuid: DebugId,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> Option<BcSymbolMapHandle> {
        // First find the PList.
        let plist_handle = self
            .fetch_file_from_all_sources(uuid, AuxDifKind::UuidMap, scope.clone(), sources.clone())
            .await?;

        let uuid_mapping = UuidMapping::parse_plist(uuid, &plist_handle.data)
            .context("Failed to parse plist")
            .map_err(|err| {
                tracing::warn!("{}: {:?}", err, err.source());
                sentry::capture_error(&*err);
                err
            })
            .ok()?;

        // Next find the BCSymbolMap.
        let symbolmap_handle = self
            .fetch_file_from_all_sources(
                uuid_mapping.original_uuid(),
                AuxDifKind::BcSymbolMap,
                scope,
                sources,
            )
            .await?;

        Some(BcSymbolMapHandle {
            file: symbolmap_handle.file.clone(),
            uuid: symbolmap_handle.uuid,
            data: symbolmap_handle.data.clone(),
        })
    }

    async fn fetch_file_from_all_sources(
        &self,
        uuid: DebugId,
        dif_kind: AuxDifKind,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
    ) -> Option<Arc<CacheHandle>> {
        let file_type = match dif_kind {
            AuxDifKind::BcSymbolMap => &[FileType::BcSymbolMap],
            AuxDifKind::UuidMap => &[FileType::UuidMap],
        };
        let files = download::list_files(
            &self.download_svc,
            &self.symstore_index_svc,
            &sources,
            file_type,
            &uuid.into(),
        )
        .await;

        let fetch_jobs = files.into_iter().map(|file_source| {
            let scope = if file_source.is_public() {
                Scope::Global
            } else {
                scope.clone()
            };
            let hub = Hub::new_from_top(Hub::current());
            hub.configure_scope(|scope| {
                scope.set_tag("auxdif.debugid", uuid);
                scope.set_extra("auxdif.kind", dif_kind.to_string().into());
                scope.set_extra("auxdif.source", file_source.source_metric_key().into());
            });
            let request = FetchFileRequest {
                file_source,
                uuid,
                kind: dif_kind,
                download_svc: self.download_svc.clone(),
            };
            let cache_key = CacheKey::from_scoped_file(&scope, &request.file_source);
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
