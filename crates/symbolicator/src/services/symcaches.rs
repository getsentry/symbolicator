use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use futures::future::BoxFuture;
use sentry::{configure_scope, Hub, SentryFutureExt};
use symbolic::common::{Arch, ByteView};
use symbolic::debuginfo::Object;
use symbolic::symcache::{self, SymCache, SymCacheWriter};
use thiserror::Error;

use crate::cache::{Cache, CacheStatus};
use crate::services::bitcode::{BcSymbolMapHandle, BitcodeService};
use crate::services::cacher::{CacheItemRequest, CacheKey, CachePath, CacheVersions, Cacher};
use crate::services::objects::{
    FindObject, FoundObject, ObjectError, ObjectHandle, ObjectMetaHandle, ObjectPurpose,
    ObjectsActor,
};
use crate::sources::{FileType, SourceConfig};
use crate::types::{
    AllObjectCandidates, ObjectFeatures, ObjectId, ObjectType, ObjectUseInfo, Scope,
};
use crate::utils::futures::{m, measure, CancelOnDrop};
use crate::utils::sentry::ConfigureScope;

use super::shared_cache::SharedCacheService;

/// This marker string is appended to symcaches to indicate that they were created using a `BcSymbolMap`.
const SYMBOLMAP_MARKER: &[u8] = b"WITH_SYMBOLMAP";

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
const SYMCACHE_VERSIONS: CacheVersions = CacheVersions {
    current: 1,
    fallbacks: &[0],
};
static_assert!(symbolic::symcache::SYMCACHE_VERSION == 7);

/// Errors happening while generating a symcache.
#[derive(Debug, Error)]
pub enum SymCacheError {
    #[error("failed to write symcache")]
    Io(#[from] io::Error),

    #[error("failed to download object")]
    Fetching(#[source] ObjectError),

    #[error("failed to parse symcache")]
    Parsing(#[source] symcache::SymCacheError),

    #[error("failed to write symcache")]
    Writing(#[source] symcache::SymCacheError),

    #[error("malformed symcache file")]
    Malformed,

    #[error("failed to parse object")]
    ObjectParsing(#[source] ObjectError),

    #[error("failed to handle auxiliary BCSymbolMap file")]
    BcSymbolMapError(#[source] Error),

    #[error("symcache building took too long")]
    Timeout,

    #[error("computation was canceled internally")]
    Canceled,
}

#[derive(Clone, Debug)]
pub struct SymCacheActor {
    symcaches: Arc<Cacher<FetchSymCacheInternal>>,
    objects: ObjectsActor,
    bitcode_svc: BitcodeService,
    threadpool: tokio::runtime::Handle,
}

impl SymCacheActor {
    pub fn new(
        cache: Cache,
        shared_cache_svc: Arc<SharedCacheService>,
        objects: ObjectsActor,
        bitcode_svc: BitcodeService,
        threadpool: tokio::runtime::Handle,
    ) -> Self {
        SymCacheActor {
            symcaches: Arc::new(Cacher::new(cache, shared_cache_svc)),
            objects,
            bitcode_svc,
            threadpool,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SymCacheFile {
    data: ByteView<'static>,
    features: ObjectFeatures,
    status: CacheStatus,
    arch: Arch,
    candidates: AllObjectCandidates,
}

impl SymCacheFile {
    pub fn parse(&self) -> Result<Option<SymCache<'_>>, SymCacheError> {
        match &self.status {
            CacheStatus::Positive => Ok(Some(
                SymCache::parse(&self.data).map_err(SymCacheError::Parsing)?,
            )),
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed(_) => Err(SymCacheError::Malformed),
            // If the cache entry is for a cache specific error, it must be
            // from a previous symcache conversion attempt.
            CacheStatus::CacheSpecificError(_) => Err(SymCacheError::Malformed),
        }
    }

    /// Returns the architecture of this symcache.
    pub fn arch(&self) -> Arch {
        self.arch
    }

    /// Returns the features of the object file this symcache was constructed from.
    pub fn features(&self) -> ObjectFeatures {
        self.features
    }

    /// Returns the list of DIFs which were searched for this symcache.
    pub fn candidates(&self) -> AllObjectCandidates {
        self.candidates.clone()
    }
}

#[derive(Clone, Debug)]
struct FetchSymCacheInternal {
    /// The external request, as passed into [`SymCacheActor::fetch`].
    request: FetchSymCache,

    /// The objects actor, used to fetch original DIF objects from.
    objects_actor: ObjectsActor,

    /// The result of fetching the BcSymbolMap
    bcsymbolmap_handle: Option<BcSymbolMapHandle>,

    /// ObjectMeta handle of the original DIF object to fetch.
    object_meta: Arc<ObjectMetaHandle>,

    /// Thread pool on which to spawn the symcache computation.
    threadpool: tokio::runtime::Handle,

    /// The object candidates from which [`FetchSymCacheInternal::object_meta`] was chosen.
    ///
    /// This needs to be returned back with the symcache result and is only being passed
    /// through here as callers to the SymCacheActer want to have this info.
    candidates: AllObjectCandidates,
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
    bcsymbolmap_handle: Option<BcSymbolMapHandle>,
    threadpool: tokio::runtime::Handle,
) -> Result<CacheStatus, SymCacheError> {
    let object_handle = objects_actor
        .fetch(object_meta.clone())
        .await
        .map_err(SymCacheError::Fetching)?;

    // The original has a download error so the sym cache entry should just be negative.
    if matches!(object_handle.status(), &CacheStatus::CacheSpecificError(_)) {
        return Ok(CacheStatus::Negative);
    }

    if object_handle.status() != &CacheStatus::Positive {
        return Ok(object_handle.status().clone());
    }

    let compute_future = async move {
        let status = match write_symcache(&path, &*object_handle, bcsymbolmap_handle) {
            Ok(_) => CacheStatus::Positive,
            Err(err) => {
                log::warn!("Failed to write symcache: {}", err);
                sentry::capture_error(&err);
                CacheStatus::Malformed(err.to_string())
            }
        };
        Ok(status)
    };

    CancelOnDrop::new(threadpool.spawn(compute_future.bind_hub(Hub::current())))
        .await
        .unwrap_or(Err(SymCacheError::Canceled))
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCacheFile;
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
            self.bcsymbolmap_handle.clone(),
            self.threadpool.clone(),
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
        let had_symbolmap = data.ends_with(SYMBOLMAP_MARKER);
        SymCache::parse(data)
            .map(|_symcache| {
                // NOTE: we do *not* check for the `is_latest` version here.
                // If the symcache is parsable, we want to use even outdated versions.
                had_symbolmap == self.bcsymbolmap_handle.is_some()
            })
            .unwrap_or(false)
    }

    fn load(
        &self,
        _scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        // TODO: Figure out if this double-parsing could be avoided
        let arch = SymCache::parse(&data)
            .map(|cache| cache.arch())
            .unwrap_or_default();

        let mut candidates = self.candidates.clone(); // yuk!
        candidates.set_debug(
            self.object_meta.source_id().clone(),
            &self.object_meta.uri(),
            ObjectUseInfo::from_derived_status(&status, self.object_meta.status()),
        );

        SymCacheFile {
            data,
            features: self.object_meta.features(),
            status,
            arch,
            candidates,
        }
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

impl SymCacheActor {
    pub async fn fetch(
        &self,
        request: FetchSymCache,
    ) -> Result<Arc<SymCacheFile>, Arc<SymCacheError>> {
        let FoundObject { meta, candidates } = self
            .objects
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .await
            .map_err(|e| Arc::new(SymCacheError::Fetching(e)))?;

        match meta {
            Some(handle) => {
                // TODO: while there is some caching *internally* in the bitcode_svc, the *complete*
                // fetch request is not cached
                let bcsymbolmap_handle = match handle.object_id().debug_id {
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
                };

                self.symcaches
                    .compute_memoized(FetchSymCacheInternal {
                        request,
                        objects_actor: self.objects.clone(),
                        bcsymbolmap_handle,
                        object_meta: handle,
                        threadpool: self.threadpool.clone(),
                        candidates,
                    })
                    .await
            }
            None => Ok(Arc::new(SymCacheFile {
                data: ByteView::from_slice(b""),
                features: ObjectFeatures::default(),
                status: CacheStatus::Negative,
                arch: Arch::Unknown,
                candidates,
            })),
        }
    }
}

/// Computes and writes the symcache.
///
/// It is assumed both the `object_handle` contains a positive cache.  The
/// `bcsymbolmap_handle` can only exist for a positive cache so does not have this issue.
#[tracing::instrument(skip_all)]
fn write_symcache(
    path: &Path,
    object_handle: &ObjectHandle,
    bcsymbolmap_handle: Option<BcSymbolMapHandle>,
) -> Result<(), SymCacheError> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_symcache"));
        object_handle.to_scope(scope);
    });

    let mut symbolic_object = object_handle
        .parse()
        .map_err(SymCacheError::ObjectParsing)?
        .unwrap();
    if let Object::MachO(ref mut macho) = symbolic_object {
        if let Some(ref handle) = bcsymbolmap_handle {
            let bcsymbolmap = handle
                .bc_symbol_map()
                .map_err(SymCacheError::BcSymbolMapError)?;
            log::debug!(
                "Adding BCSymbolMap {} to dSYM {}",
                handle.uuid,
                object_handle
            );
            macho.load_symbolmap(bcsymbolmap);
        }
    }

    let file = File::create(&path)?;
    let mut writer = BufWriter::new(file);

    log::debug!("Converting symcache for {}", object_handle.cache_key());

    SymCacheWriter::write_object(&symbolic_object, &mut writer).map_err(SymCacheError::Writing)?;

    let mut file = writer.into_inner().map_err(io::Error::from)?;

    if bcsymbolmap_handle.is_some() {
        file.flush()?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(SYMBOLMAP_MARKER)?;
    }
    file.sync_all()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use symbolic::common::DebugId;
    use uuid::Uuid;

    use super::*;
    use crate::cache::Caches;
    use crate::config::{CacheConfigs, Config};
    use crate::services::bitcode::BitcodeService;
    use crate::services::DownloadService;
    use crate::sources::{
        CommonSourceConfig, DirectoryLayoutType, FilesystemSourceConfig, SourceConfig, SourceId,
    };
    use crate::test::{self, fixture};

    /// Creates a `SymCacheActor` with the given cache directory
    /// and timeout for download cache misses.
    async fn symcache_actor(cache_dir: PathBuf, timeout: Duration) -> SymCacheActor {
        let mut cache_config = CacheConfigs::default();
        cache_config.downloaded.retry_misses_after = Some(timeout);

        let config = Arc::new(Config {
            cache_dir: Some(cache_dir),
            connect_to_reserved_ips: true,
            caches: cache_config,
            ..Default::default()
        });

        let cpu_pool = tokio::runtime::Handle::current();
        let caches = Caches::from_config(&config).unwrap();
        caches.clear_tmp(&config).unwrap();
        let downloader = DownloadService::new(config);
        let shared_cache = Arc::new(SharedCacheService::new(None).await);
        let objects = ObjectsActor::new(
            caches.object_meta,
            caches.objects,
            shared_cache.clone(),
            downloader.clone(),
        );
        let bitcode = BitcodeService::new(caches.auxdifs, shared_cache.clone(), downloader);

        SymCacheActor::new(caches.symcaches, shared_cache, objects, bitcode, cpu_pool)
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
        let symcache_file = symcache_actor.fetch(fetch_symcache.clone()).await.unwrap();
        let symcache = symcache_file.parse().unwrap().unwrap();
        let line_info = symcache.lookup(0x5a75).unwrap().next().unwrap().unwrap();
        assert_eq!(line_info.filename(), "__hidden#42_");
        assert_eq!(line_info.symbol(), "__hidden#0_");

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
        let symcache_file = symcache_actor.fetch(fetch_symcache.clone()).await.unwrap();
        let symcache = symcache_file.parse().unwrap().unwrap();
        let line_info = symcache.lookup(0x5a75).unwrap().next().unwrap().unwrap();
        assert_eq!(line_info.filename(), "__hidden#42_");
        assert_eq!(line_info.symbol(), "__hidden#0_");

        // Sleep long enough for the negative cache entry to become invalid.
        std::thread::sleep(TIMEOUT);

        // Create the symcache for the third time. This time, the bcsymbolmap is downloaded and the names in the
        // symcache are unobfuscated.
        let symcache_file = symcache_actor.fetch(fetch_symcache.clone()).await.unwrap();
        let symcache = symcache_file.parse().unwrap().unwrap();
        let line_info = symcache.lookup(0x5a75).unwrap().next().unwrap().unwrap();
        assert_eq!(line_info.filename(), "Sources/Sentry/SentryMessage.m");
        assert_eq!(line_info.symbol(), "-[SentryMessage initWithFormatted:]");
    }
}
