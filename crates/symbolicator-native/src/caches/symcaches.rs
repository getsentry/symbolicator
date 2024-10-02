use std::fmt::Write;
use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;

use futures::future::BoxFuture;
use sentry::{Hub, SentryFutureExt};
use tempfile::NamedTempFile;

use symbolic::common::{ByteView, SelfCell};
use symbolic::symcache::{SymCache, SymCacheConverter};
use symbolicator_service::caches::versions::SYMCACHE_VERSIONS;
use symbolicator_service::caching::{
    Cache, CacheEntry, CacheError, CacheItemRequest, CacheVersions, Cacher, SharedCacheRef,
};
use symbolicator_service::objects::{
    CandidateStatus, FindObject, ObjectHandle, ObjectMetaHandle, ObjectPurpose, ObjectsActor,
};
use symbolicator_service::types::Scope;
use symbolicator_service::utils::sentry::ConfigureScope;
use symbolicator_sources::{FileType, ObjectId, ObjectType, SourceConfig};

use super::bitcode::{BcSymbolMapHandle, BitcodeService};
use super::derived::{derive_from_object_handle, DerivedCache};
use super::il2cpp::{Il2cppHandle, Il2cppService};

pub type OwnedSymCache = SelfCell<ByteView<'static>, SymCache<'static>>;

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
        shared_cache: SharedCacheRef,
        objects: ObjectsActor,
        bitcode_svc: BitcodeService,
        il2cpp_svc: Il2cppService,
    ) -> Self {
        SymCacheActor {
            symcaches: Arc::new(Cacher::new(cache, shared_cache)),
            objects,
            bitcode_svc,
            il2cpp_svc,
        }
    }
}

#[derive(Clone, Debug)]
struct FetchSymCacheInternal {
    /// The objects actor, used to fetch original DIF objects from.
    objects_actor: ObjectsActor,

    /// Secondary sources to use when creating a SymCache.
    secondary_sources: SecondarySymCacheSources,

    /// ObjectMeta handle of the original DIF object to fetch.
    object_meta: Arc<ObjectMetaHandle>,
}

#[tracing::instrument(name = "compute_symcache", skip_all)]
async fn compute_symcache(
    temp_file: &mut NamedTempFile,
    objects_actor: &ObjectsActor,
    object_meta: Arc<ObjectMetaHandle>,
    secondary_sources: &SecondarySymCacheSources,
) -> CacheEntry {
    let object_handle = objects_actor.fetch(object_meta).await?;

    write_symcache(temp_file.as_file_mut(), &object_handle, secondary_sources)
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = OwnedSymCache;

    const VERSIONS: CacheVersions = SYMCACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(compute_symcache(
            temp_file,
            &self.objects_actor,
            self.object_meta.clone(),
            &self.secondary_sources,
        ))
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        SelfCell::try_new(data, |p| unsafe {
            SymCache::parse(&*p).map_err(|e| {
                let object_meta = &self.object_meta;
                tracing::error!(
                    scope = %object_meta.scope(),
                    cache_key = object_meta.cache_key().metadata(),
                    debug_id = ?object_meta.object_id().debug_id,
                    code_id = ?object_meta.object_id().code_id,
                    error = %e
                );
                CacheError::InternalError
            })
        })
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
    pub async fn fetch(&self, request: FetchSymCache) -> DerivedCache<OwnedSymCache> {
        let found_object = self
            .objects
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .await;

        derive_from_object_handle(found_object, CandidateStatus::Debug, |handle| async move {
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
                            .bind_hub(Hub::new_from_top(Hub::current()))
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
                            .bind_hub(Hub::new_from_top(Hub::current()))
                            .await
                    }
                    None => None,
                }
            };

            let (bcsymbolmap_handle, il2cpp_handle) =
                futures::future::join(fetch_bcsymbolmap, fetch_il2cpp).await;

            let mut builder = handle.cache_key_builder();
            if let Some(handle) = &bcsymbolmap_handle {
                builder.write_str("\nbcsymbolmap:\n").unwrap();
                builder.write_file_meta(&handle.file).unwrap();
            }
            if let Some(handle) = &il2cpp_handle {
                builder.write_str("\nil2cpp:\n").unwrap();
                builder.write_file_meta(&handle.file).unwrap();
            }

            let cache_key = builder.build();

            let secondary_sources = SecondarySymCacheSources {
                bcsymbolmap_handle,
                il2cpp_handle,
            };

            let request = FetchSymCacheInternal {
                objects_actor: self.objects.clone(),
                secondary_sources,
                object_meta: Arc::clone(&handle),
            };
            self.symcaches.compute_memoized(request, cache_key).await
        })
        .await
    }
}

/// Encapsulation of all the source artifacts that are being used to create SymCaches.
#[derive(Clone, Debug, Default)]
struct SecondarySymCacheSources {
    bcsymbolmap_handle: Option<BcSymbolMapHandle>,
    il2cpp_handle: Option<Il2cppHandle>,
}

/// Computes and writes the symcache.
///
/// It is assumed that the `object_handle` contains a positive cache.
/// Any secondary source can only exist for a positive cache so does not have this issue.
#[tracing::instrument(skip_all)]
fn write_symcache(
    file: &mut File,
    object_handle: &ObjectHandle,
    secondary_sources: &SecondarySymCacheSources,
) -> CacheEntry {
    object_handle.configure_scope();

    let symbolic_object = object_handle.object();

    let bcsymbolmap_transformer = match secondary_sources.bcsymbolmap_handle.as_ref() {
        Some(handle) => {
            let bcsymbolmap = handle.bc_symbol_map().map_err(|e| {
                // FIXME(swatinem): this should really be an InternalError?

                let dynerr: &dyn std::error::Error = e.as_ref(); // tracing expects a `&dyn Error`
                tracing::error!(error = dynerr, "Failed to parse BcSymbolMap");

                CacheError::Malformed(e.to_string())
            })?;
            tracing::debug!(
                "Adding BCSymbolMap {} to dSYM {}",
                handle.uuid,
                object_handle
            );
            Some(bcsymbolmap)
        }
        None => None,
    };
    let linemapping_transformer = match secondary_sources.il2cpp_handle.as_ref() {
        Some(handle) => {
            let mapping = handle.line_mapping().ok_or_else(|| {
                tracing::error!("cached il2cpp LineMapping should have been parsable");
                CacheError::InternalError
            })?;
            Some(mapping)
        }
        None => None,
    };

    tracing::debug!("Converting symcache for {}", object_handle.cache_key);

    let mut converter = SymCacheConverter::new();

    if let Some(bcsymbolmap) = bcsymbolmap_transformer {
        converter.add_transformer(bcsymbolmap);
    }
    if let Some(linemapping) = linemapping_transformer {
        tracing::debug!("Adding il2cpp line mapping to object {}", object_handle);
        converter.add_transformer(linemapping);
    }

    converter.process_object(symbolic_object).map_err(|e| {
        let dynerr: &dyn std::error::Error = &e; // tracing expects a `&dyn Error`
        tracing::error!(error = dynerr, "Could not process SymCache");

        CacheError::Malformed(e.to_string())
    })?;

    let mut writer = BufWriter::new(file);
    converter.serialize(&mut writer)?;
    let file = writer.into_inner().map_err(io::Error::from)?;
    file.sync_all()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    use symbolic::common::{DebugId, Uuid};
    use symbolicator_service::caching::Caches;
    use symbolicator_service::config::{CacheConfigs, Config};
    use symbolicator_service::download::DownloadService;
    use symbolicator_sources::{
        CommonSourceConfig, DirectoryLayoutType, FilesystemSourceConfig, SourceId,
    };
    use symbolicator_test::{self as test, fixture};

    use super::*;

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

        let caches = Caches::from_config(&config).unwrap();
        caches.clear_tmp(&config).unwrap();
        let downloader = DownloadService::new(&config, tokio::runtime::Handle::current());
        let shared_cache = SharedCacheRef::default();
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

        const TIMEOUT: Duration = Duration::from_secs(2);

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
        let symcache = symcache_actor
            .fetch(fetch_symcache.clone())
            .await
            .cache
            .unwrap();

        let sl = symcache.get().lookup(0x5a75).next().unwrap();
        assert_eq!(
            sl.file().unwrap().full_path(),
            "__hidden#41_/__hidden#41_/__hidden#42_"
        );
        assert_eq!(sl.function().name(), "__hidden#0_");
        drop(symcache);

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
        // still be cached and the SymCacheActor should make no attempt to download it. Therefore, the names should
        // be obfuscated like before.
        let symcache = symcache_actor
            .fetch(fetch_symcache.clone())
            .await
            .cache
            .unwrap();

        let sl = symcache.get().lookup(0x5a75).next().unwrap();
        assert_eq!(
            sl.file().unwrap().full_path(),
            "__hidden#41_/__hidden#41_/__hidden#42_"
        );
        assert_eq!(sl.function().name(), "__hidden#0_");
        drop(symcache);

        // Sleep long enough for the negative cache entry to become invalid.
        std::thread::sleep(TIMEOUT);

        // Create the symcache for the third time. This time, the bcsymbolmap is downloaded and the names in the
        // symcache are unobfuscated.
        let symcache = symcache_actor.fetch(fetch_symcache).await.cache.unwrap();

        let sl = symcache.get().lookup(0x5a75).next().unwrap();
        assert_eq!(
            sl.file().unwrap().full_path(),
            "/Users/philipphofmann/git-repos/sentry-cocoa/Sources/Sentry/SentryMessage.m"
        );
        assert_eq!(sl.function().name(), "-[SentryMessage initWithFormatted:]");
    }
}
