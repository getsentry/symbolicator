use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use futures::future::BoxFuture;
use thiserror::Error;

use symbolic::common::{ByteView, SelfCell};
use symbolic::symcache::{self, SymCache, SymCacheConverter};
use symbolicator_sources::{FileType, ObjectId, ObjectType, SourceConfig};

use crate::cache::{
    cache_entry_as_cache_status, cache_entry_from_cache_status, Cache, CacheEntry, CacheError,
    CacheStatus, ExpirationTime,
};
use crate::services::bitcode::BitcodeService;
use crate::services::cacher::{CacheItemRequest, CacheKey, CacheVersions, Cacher};
use crate::services::objects::{
    FindObject, FoundObject, ObjectError, ObjectHandle, ObjectMetaHandle, ObjectPurpose,
    ObjectsActor,
};
use crate::types::{AllObjectCandidates, ObjectFeatures, ObjectUseInfo, Scope};
use crate::utils::futures::{m, measure};
use crate::utils::sentry::ConfigureScope;

use self::markers::{SecondarySymCacheSources, SymCacheMarkers};

use super::download::DownloadError;
use super::il2cpp::Il2cppService;
use super::shared_cache::SharedCacheService;

mod markers;

/// The supported symcache versions.
///
/// # How to version
///
/// The initial "unversioned" version is `0`.
/// Whenever we want to increase the version in order to re-generate stale/broken
/// symcaches, we need to:
///
/// * increase the `current` version.
/// * prepend the `current` version to the `fallbacks`.
/// * it is also possible to skip a version, in case a broken deploy needed to
///   be reverted which left behind broken symcaches.
///
/// In case a symbolic update increased its own internal format version, bump the
/// symcache file version as described above, and update the static assertion.
///
/// # Version History
///
/// - `5`: Proactive bump, as a bug in shared cache could have potentially
///   uploaded `v2` cache files as `v3` (and later `v4`) erroneously.
///
/// - `4`: An updated symbolic symcache that uses a LEB128 prefixed string table.
///
/// - `3`: Another round of fixes in symcache generation:
///        - fixes problems with split inlinees and inlinees appearing twice in the call chain
///        - undecorate Windows C-decorated symbols in symcaches
///
/// - `2`: Tons of fixes/improvements in symcache generation:
///        - fixed problems with DWARF functions that have the
///          same line records for different inline hierarchy
///        - fixed problems with PDB where functions have line records that don't belong to them
///        - fixed problems with PDB/DWARF when parent functions don't have matching line records
///        - using a new TypeFormatter for PDB that can pretty-print function arguments
///
/// - `1`: New binary format based on instruction addr lookup.
const SYMCACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 5,
    fallbacks: &[4],
};
static_assert!(symbolic::symcache::SYMCACHE_VERSION == 8);

pub type OwnedSymCache = SelfCell<ByteView<'static>, SymCache<'static>>;

fn parse_symcache_owned(byteview: ByteView<'static>) -> Result<OwnedSymCache, CacheError> {
    SelfCell::try_new(byteview, |p| unsafe {
        SymCache::parse(&*p).map_err(|e| {
            tracing::error!(error = %e);
            CacheError::InternalError
        })
    })
}

/// Errors happening while generating a symcache.
#[derive(Debug, Error)]
pub enum SymCacheError {
    #[error("failed to write symcache")]
    Io(#[from] io::Error),

    #[error("failed to download object")]
    Fetching(#[source] ObjectError),

    #[error("failed to parse symcache")]
    Parsing(#[source] symcache::Error),

    #[error("failed to write symcache")]
    Writing(#[source] symcache::Error),

    #[error("malformed symcache file")]
    Malformed,

    #[error("failed to parse object")]
    ObjectParsing(#[source] ObjectError),

    #[error("failed to handle auxiliary BCSymbolMap file")]
    BcSymbolMapError(#[source] Error),

    #[error("failed to handle auxiliary il2cpp line mapping file")]
    Il2cppError(#[source] Error),

    #[error("symcache building took too long")]
    Timeout,
}

impl From<&SymCacheError> for CacheError {
    fn from(error: &SymCacheError) -> Self {
        match error {
            SymCacheError::Io(e) => {
                tracing::error!(error = %e, "failed to write symcache");
                Self::InternalError
            }
            SymCacheError::Parsing(e) => {
                tracing::error!(error = %e, "failed to parse symcache");
                Self::InternalError
            }
            SymCacheError::Writing(e) => {
                tracing::error!(error = %e, "failed to write symcache");
                Self::InternalError
            }
            SymCacheError::ObjectParsing(e) => {
                tracing::error!(error = %e, "failed to parse object");
                Self::InternalError
            }
            SymCacheError::BcSymbolMapError(e) => {
                tracing::error!(error = %e, "failed to handle auxiliary BCSymbolMap file");
                Self::InternalError
            }
            SymCacheError::Il2cppError(e) => {
                tracing::error!(error = %e, "failed to handle auxiliary il2cpp line mapping file");
                Self::InternalError
            }
            SymCacheError::Fetching(ref e) => Self::DownloadError(e.to_string()),
            SymCacheError::Malformed => Self::Malformed(String::new()),
            SymCacheError::Timeout => Self::Timeout(Duration::default()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SymCacheActor {
    symcaches: Arc<Cacher<FetchSymCacheInternal>>,
    objects: ObjectsActor,
    bitcode_svc: BitcodeService,
    il2cpp_svc: Il2cppService,
}

impl SymCacheActor {
    pub fn new(
        cache: Cache,
        shared_cache_svc: Arc<SharedCacheService>,
        objects: ObjectsActor,
        bitcode_svc: BitcodeService,
        il2cpp_svc: Il2cppService,
    ) -> Self {
        SymCacheActor {
            symcaches: Arc::new(Cacher::new(cache, shared_cache_svc)),
            objects,
            bitcode_svc,
            il2cpp_svc,
        }
    }
}

#[derive(Clone, Debug)]
struct FetchSymCacheInternal {
    /// The external request, as passed into [`SymCacheActor::fetch`].
    request: FetchSymCache,

    /// The objects actor, used to fetch original DIF objects from.
    objects_actor: ObjectsActor,

    /// Secondary sources to use when creating a SymCache.
    secondary_sources: SecondarySymCacheSources,

    /// ObjectMeta handle of the original DIF object to fetch.
    object_meta: Arc<ObjectMetaHandle>,
}

/// Fetches the needed DIF objects and spawns symcache computation.
///
/// Required DIF objects are fetched from the objects actor in the current executor, once
/// DIFs have been retrieved it spawns the symcache computation onto the provided
/// threadpool.
///
/// This is the actual implementation of [`CacheItemRequest::compute`] for
/// [`FetchSymCacheInternal`] but outside of the trait so it can be written as async/await
/// code.
#[tracing::instrument(name = "compute_symcache", skip_all)]
async fn fetch_difs_and_compute_symcache(
    path: PathBuf,
    object_meta: Arc<ObjectMetaHandle>,
    objects_actor: ObjectsActor,
    secondary_sources: SecondarySymCacheSources,
) -> Result<CacheStatus, SymCacheError> {
    let object_handle = objects_actor
        .fetch(object_meta.clone())
        .await
        .map_err(SymCacheError::Fetching)?;

    let status = object_handle.status();

    // The original has a download error so the sym cache entry should be marked as a fetch error
    if let Some(_download_error) = DownloadError::from_cache(status) {
        // FIXME: this should be:
        //return Err(SymCacheError::Fetching(download_error.into()));
        // but instead we have to return an `Ok`, otherwise the `candidate` info will be lost
        // somwhere along the path of the symcache request.
        return Ok(CacheStatus::Negative);
    }

    if status != &CacheStatus::Positive {
        return Ok(status.clone());
    }

    let status = match write_symcache(&path, &object_handle, secondary_sources) {
        Ok(_) => CacheStatus::Positive,
        Err(err) => {
            tracing::warn!("Failed to write symcache: {}", err);
            sentry::capture_error(&err);
            CacheStatus::Malformed(err.to_string())
        }
    };
    Ok(status)
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = CacheEntry<OwnedSymCache>;
    type Error = SymCacheError;

    const VERSIONS: CacheVersions = SYMCACHE_VERSIONS;

    fn get_cache_key(&self) -> CacheKey {
        self.object_meta.cache_key()
    }

    fn compute(&self, path: &Path) -> BoxFuture<'static, Result<CacheStatus, Self::Error>> {
        let future = fetch_difs_and_compute_symcache(
            path.to_owned(),
            self.object_meta.clone(),
            self.objects_actor.clone(),
            self.secondary_sources.clone(),
        );

        let num_sources = self.request.sources.len().to_string().into();

        let future = tokio::time::timeout(Duration::from_secs(1200), future);
        let future = measure(
            "symcaches",
            m::timed_result,
            Some(("num_sources", num_sources)),
            future,
        );
        Box::pin(async move { future.await.map_err(|_| SymCacheError::Timeout)? })
    }

    fn should_load(&self, data: &[u8]) -> bool {
        SymCache::parse(data)
            .map(|_symcache| {
                // NOTE: we do *not* check for the `is_latest` version here.
                // If the symcache is parsable, we want to use even outdated versions.

                let symcache_markers = SymCacheMarkers::parse(data);
                SymCacheMarkers::from_sources(&self.secondary_sources) == symcache_markers
            })
            .unwrap_or(false)
    }

    fn load(
        &self,
        status: CacheStatus,
        data: ByteView<'static>,
        _expiration: ExpirationTime,
    ) -> Self::Item {
        let cache_entry = cache_entry_from_cache_status(status, data);
        cache_entry.and_then(parse_symcache_owned)
    }
}

/// Information for fetching the symbols for this symcache
#[derive(Debug, Clone)]
pub struct FetchSymCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Arc<[SourceConfig]>,
    pub scope: Scope,
}

#[derive(Debug, Clone)]
pub struct FetchSymCacheResponse {
    pub cache: CacheEntry<OwnedSymCache>,
    pub candidates: AllObjectCandidates,
    pub features: ObjectFeatures,
}

impl Default for FetchSymCacheResponse {
    fn default() -> Self {
        Self {
            cache: Err(CacheError::InternalError),
            candidates: Default::default(),
            features: Default::default(),
        }
    }
}

impl SymCacheActor {
    pub async fn fetch(&self, request: FetchSymCache) -> FetchSymCacheResponse {
        let Ok(FoundObject { meta, mut candidates }) = self
            .objects
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .await else {
                return FetchSymCacheResponse::default();
        };

        match meta {
            Some(handle) => {
                // TODO: while there is some caching *internally* in the bitcode_svc, the *complete*
                // fetch request is not cached
                let fetch_bcsymbolmap = async {
                    match handle.object_id().debug_id {
                        Some(debug_id) => {
                            self.bitcode_svc
                                .fetch_bcsymbolmap(
                                    debug_id,
                                    handle.scope().clone(),
                                    request.sources.clone(),
                                )
                                .await
                        }
                        None => None,
                    }
                };

                let fetch_il2cpp = async {
                    match handle.object_id().debug_id {
                        Some(debug_id) => {
                            tracing::trace!("Fetching line mapping");
                            self.il2cpp_svc
                                .fetch_line_mapping(
                                    handle.object_id(),
                                    debug_id,
                                    handle.scope().clone(),
                                    request.sources.clone(),
                                )
                                .await
                        }
                        None => None,
                    }
                };

                let (bcsymbolmap_handle, il2cpp_handle) =
                    futures::future::join(fetch_bcsymbolmap, fetch_il2cpp).await;

                let secondary_sources = SecondarySymCacheSources {
                    bcsymbolmap_handle,
                    il2cpp_handle,
                };

                match self
                    .symcaches
                    .compute_memoized(FetchSymCacheInternal {
                        request,
                        objects_actor: self.objects.clone(),
                        secondary_sources,
                        object_meta: Arc::clone(&handle),
                    })
                    .await
                {
                    Ok(symcache_file) => {
                        candidates.set_debug(
                            handle.source_id(),
                            &handle.uri(),
                            ObjectUseInfo::from_derived_status(
                                &cache_entry_as_cache_status(&symcache_file),
                                handle.status(),
                            ),
                        );

                        FetchSymCacheResponse {
                            cache: Arc::try_unwrap(symcache_file)
                                .unwrap_or_else(|arc| (*arc).clone()),
                            candidates,
                            features: handle.features(),
                        }
                    }
                    Err(e) => FetchSymCacheResponse {
                        cache: Err(e.as_ref().into()),
                        candidates,
                        features: ObjectFeatures::default(),
                    },
                }
            }
            None => FetchSymCacheResponse {
                cache: Err(CacheError::NotFound),
                candidates,
                features: ObjectFeatures::default(),
            },
        }
    }
}

/// Computes and writes the symcache.
///
/// It is assumed that the `object_handle` contains a positive cache.
/// Any secondary source can only exist for a positive cache so does not have this issue.
#[tracing::instrument(skip_all)]
fn write_symcache(
    path: &Path,
    object_handle: &ObjectHandle,
    secondary_sources: SecondarySymCacheSources,
) -> Result<(), SymCacheError> {
    object_handle.configure_scope();

    let symbolic_object = object_handle
        .parse()
        .map_err(SymCacheError::ObjectParsing)?
        .unwrap();

    let markers = SymCacheMarkers::from_sources(&secondary_sources);

    let bcsymbolmap_transformer = match secondary_sources.bcsymbolmap_handle {
        Some(ref handle) => {
            let bcsymbolmap = handle
                .bc_symbol_map()
                .map_err(SymCacheError::BcSymbolMapError)?;
            tracing::debug!(
                "Adding BCSymbolMap {} to dSYM {}",
                handle.uuid,
                object_handle
            );
            Some(bcsymbolmap)
        }
        None => None,
    };
    let linemapping_transformer = match secondary_sources.il2cpp_handle {
        Some(handle) => {
            let il2cpp = handle.line_mapping().map_err(SymCacheError::Il2cppError)?;
            tracing::debug!(
                "Adding il2cpp line mapping {} to object {}",
                handle.debug_id,
                object_handle
            );
            Some(il2cpp)
        }
        None => None,
    };

    tracing::debug!("Converting symcache for {}", object_handle.cache_key());

    let mut converter = SymCacheConverter::new();

    if let Some(bcsymbolmap) = bcsymbolmap_transformer {
        converter.add_transformer(bcsymbolmap);
    }
    if let Some(linemapping) = linemapping_transformer {
        converter.add_transformer(linemapping);
    }

    converter
        .process_object(&symbolic_object)
        .map_err(SymCacheError::Writing)?;

    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    converter
        .serialize(&mut writer)
        .map_err(SymCacheError::Io)?;

    let mut file = writer.into_inner().map_err(io::Error::from)?;

    markers.write_to(&mut file)?;

    file.sync_all()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use symbolic::common::{DebugId, Uuid};

    use super::*;
    use crate::cache::Caches;
    use crate::config::{CacheConfigs, Config};
    use crate::services::bitcode::BitcodeService;
    use crate::services::DownloadService;
    use crate::test::{self, fixture};
    use symbolicator_sources::{
        CommonSourceConfig, DirectoryLayoutType, FilesystemSourceConfig, SourceConfig, SourceId,
    };

    /// Creates a `SymCacheActor` with the given cache directory
    /// and timeout for download cache misses.
    async fn symcache_actor(cache_dir: PathBuf, timeout: Duration) -> SymCacheActor {
        let mut cache_config = CacheConfigs::default();
        cache_config.downloaded.retry_misses_after = Some(timeout);

        let config = Config {
            cache_dir: Some(cache_dir),
            connect_to_reserved_ips: true,
            caches: cache_config,
            ..Default::default()
        };

        let runtime = tokio::runtime::Handle::current();
        let caches = Caches::from_config(&config).unwrap();
        caches.clear_tmp(&config).unwrap();
        let downloader = DownloadService::new(&config, runtime.clone());
        let shared_cache = Arc::new(SharedCacheService::new(None, runtime.clone()).await);
        let objects = ObjectsActor::new(
            caches.object_meta,
            caches.objects,
            shared_cache.clone(),
            downloader.clone(),
        );
        let bitcode = BitcodeService::new(caches.auxdifs, shared_cache.clone(), downloader.clone());
        let il2cpp = Il2cppService::new(caches.il2cpp, shared_cache.clone(), downloader);

        SymCacheActor::new(caches.symcaches, shared_cache, objects, bitcode, il2cpp)
    }

    /// Tests that a symcache is regenerated when it was created without a BcSymbolMap
    /// and a BcSymbolMap has since become available.
    ///
    /// We construct a symcache 3 times under varying conditions:
    /// 1. No symbol map is not there
    /// 2. The symbol map is there, but its absence is still cached, so it is
    ///    not downloaded
    /// 3. The download cache has expired, so the symbol map is now
    ///    actually available.
    ///
    /// Lookups in the symcache should return obfuscated names in
    /// 1 and 2 and unobfuscated names in 3.
    ///
    /// 2 is specifically intended to make sure that the SymCacheActor
    /// doesn't constantly try to download the symbol map.
    #[tokio::test]
    async fn test_symcache_refresh() {
        test::setup();

        const TIMEOUT: Duration = Duration::from_secs(5);

        let cache_dir = test::tempdir();
        let symbol_dir = test::tempdir();

        // Create directories for the symbol file and the bcsymbolmap
        let macho_dir = symbol_dir.path().join("2d/10c42f591d3265b14778ba0868073f/");
        let symbol_map_dir = symbol_dir.path().join("c8/374b6d6e9634d8ae38efaa5fec424f/");

        fs::create_dir_all(&symbol_map_dir).unwrap();
        fs::create_dir_all(&macho_dir).unwrap();

        // Copy the symbol file to the temporary symbol directory
        fs::copy(
            fixture("symbols/2d10c42f-591d-3265-b147-78ba0868073f.dwarf-hidden"),
            macho_dir.join("debuginfo"),
        )
        .unwrap();

        let source = SourceConfig::Filesystem(Arc::new(FilesystemSourceConfig {
            id: SourceId::new("local"),
            path: symbol_dir.path().to_owned(),
            files: CommonSourceConfig::with_layout(DirectoryLayoutType::Unified),
        }));

        let identifier = ObjectId::from(DebugId::from_uuid(
            Uuid::parse_str("2d10c42f-591d-3265-b147-78ba0868073f").unwrap(),
        ));

        let fetch_symcache = FetchSymCache {
            object_type: ObjectType::Macho,
            identifier,
            sources: Arc::new([source]),
            scope: Scope::Global,
        };

        let symcache_actor = symcache_actor(cache_dir.path().to_owned(), TIMEOUT).await;

        // Create the symcache for the first time. Since the bcsymbolmap is not available, names in the
        // symcache will be obfuscated.
        let owned_symcache = symcache_actor
            .fetch(fetch_symcache.clone())
            .await
            .cache
            .ok()
            .unwrap();

        let symcache = owned_symcache.get();
        let sl = symcache.lookup(0x5a75).next().unwrap();
        assert_eq!(
            sl.file().unwrap().full_path(),
            "__hidden#41_/__hidden#41_/__hidden#42_"
        );
        assert_eq!(sl.function().name(), "__hidden#0_");

        // Copy the plist and bcsymbolmap to the temporary symbol directory so that the SymCacheActor can find them.
        fs::copy(
            fixture("symbols/2d10c42f-591d-3265-b147-78ba0868073f.plist"),
            macho_dir.join("uuidmap"),
        )
        .unwrap();

        fs::copy(
            fixture("symbols/c8374b6d-6e96-34d8-ae38-efaa5fec424f.bcsymbolmap"),
            symbol_map_dir.join("bcsymbolmap"),
        )
        .unwrap();

        // Create the symcache for the second time. Even though the bcsymbolmap is now available, its absence should
        // still be cached and the SymcacheActor should make no attempt to download it. Therefore, the names should
        // be obfuscated like before.
        let owned_symcache = symcache_actor
            .fetch(fetch_symcache.clone())
            .await
            .cache
            .ok()
            .unwrap();

        let symcache = owned_symcache.get();
        let sl = symcache.lookup(0x5a75).next().unwrap();
        assert_eq!(
            sl.file().unwrap().full_path(),
            "__hidden#41_/__hidden#41_/__hidden#42_"
        );
        assert_eq!(sl.function().name(), "__hidden#0_");

        // Sleep long enough for the negative cache entry to become invalid.
        std::thread::sleep(TIMEOUT);

        // Create the symcache for the third time. This time, the bcsymbolmap is downloaded and the names in the
        // symcache are unobfuscated.
        let owned_symcache = symcache_actor
            .fetch(fetch_symcache.clone())
            .await
            .cache
            .ok()
            .unwrap();

        let symcache = owned_symcache.get();
        let sl = symcache.lookup(0x5a75).next().unwrap();
        assert_eq!(
            sl.file().unwrap().full_path(),
            "/Users/philipphofmann/git-repos/sentry-cocoa/Sources/Sentry/SentryMessage.m"
        );
        assert_eq!(sl.function().name(), "-[SentryMessage initWithFormatted:]");
    }
}
