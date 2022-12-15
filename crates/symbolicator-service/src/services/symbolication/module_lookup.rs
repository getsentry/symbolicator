use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future;
use sentry::{Hub, SentryFutureExt};
use thiserror::Error;

use symbolic::debuginfo::ObjectDebugSession;
use symbolicator_sources::{FileType, ObjectType, SourceConfig};

use crate::cache::{CacheEntry, CacheError};
use crate::services::derived::DerivedCache;
use crate::services::objects::{FindObject, FindResult, ObjectHandle, ObjectPurpose, ObjectsActor};
use crate::services::ppdb_caches::{
    FetchPortablePdbCache, OwnedPortablePdbCache, PortablePdbCacheActor, PortablePdbCacheError,
};
use crate::services::symcaches::{FetchSymCache, OwnedSymCache, SymCacheActor, SymCacheError};
use crate::types::{
    AllObjectCandidates, CompleteObjectInfo, CompleteStacktrace, ObjectFeatures, ObjectFileStatus,
    RawStacktrace, Scope,
};
use crate::utils::addr::AddrMode;

use super::object_id_from_object_info;

pub fn object_file_status_from_cache_entry<T>(cache_entry: &CacheEntry<T>) -> ObjectFileStatus {
    match cache_entry {
        Ok(_) => ObjectFileStatus::Found,
        Err(CacheError::NotFound) => ObjectFileStatus::Missing,
        Err(CacheError::PermissionDenied(_) | CacheError::DownloadError(_)) => {
            ObjectFileStatus::FetchingFailed
        }
        Err(CacheError::Timeout(_)) => ObjectFileStatus::Timeout,
        Err(CacheError::Malformed(_)) => ObjectFileStatus::Malformed,
        Err(CacheError::InternalError) => ObjectFileStatus::Other,
    }
}

#[derive(Debug, Error)]
pub enum CacheFileError {
    #[error(transparent)]
    SymCache(#[from] Arc<SymCacheError>),
    #[error(transparent)]
    PortablePdbCache(#[from] Arc<PortablePdbCacheError>),
}

#[derive(Debug, Clone)]
pub enum CacheFileEntry {
    SymCache(OwnedSymCache),
    PortablePdbCache(OwnedPortablePdbCache),
}

#[derive(Debug, Clone)]
pub struct CacheFile {
    file: CacheEntry<CacheFileEntry>,
    candidates: AllObjectCandidates,
    features: ObjectFeatures,
}

#[derive(Debug, Clone)]
pub struct CacheLookupResult<'a> {
    pub module_index: usize,
    pub object_info: &'a CompleteObjectInfo,
    pub cache: &'a CacheEntry<CacheFileEntry>,
    pub relative_addr: Option<u64>,
}

impl<'a> CacheLookupResult<'a> {
    /// The preferred [`AddrMode`] for this lookup.
    ///
    /// For the symbolicated frame, we generally switch to absolute reporting of addresses. This is
    /// not done for images mounted at `0` because, for instance, WASM does not have a unified
    /// address space and so it is not possible for us to absolutize addresses.
    pub fn preferred_addr_mode(&self) -> AddrMode {
        if self.object_info.supports_absolute_addresses() {
            AddrMode::Abs
        } else {
            AddrMode::Rel(self.module_index)
        }
    }

    /// Exposes an address consistent with [`preferred_addr_mode`](Self::preferred_addr_mode).
    pub fn expose_preferred_addr(&self, addr: u64) -> u64 {
        if self.object_info.supports_absolute_addresses() {
            self.object_info.rel_to_abs_addr(addr).unwrap_or(0)
        } else {
            addr
        }
    }
}

struct ModuleEntry {
    module_index: usize,
    object_info: CompleteObjectInfo,
    cache: CacheEntry<CacheFileEntry>,
    source_object: CacheEntry<Arc<ObjectHandle>>,
}

pub struct ModuleLookup {
    modules: Vec<ModuleEntry>,
    scope: Scope,
    sources: Arc<[SourceConfig]>,
}

impl ModuleLookup {
    /// Creates a new [`ModuleLookup`] out of the given module iterator.
    pub fn new<I>(scope: Scope, sources: Arc<[SourceConfig]>, iter: I) -> Self
    where
        I: IntoIterator<Item = CompleteObjectInfo>,
    {
        let mut modules: Vec<_> = iter
            .into_iter()
            .enumerate()
            .map(|(module_index, object_info)| ModuleEntry {
                module_index,
                object_info,
                cache: Err(CacheError::NotFound),
                source_object: Err(CacheError::NotFound),
            })
            .collect();

        modules.sort_by_key(|entry| entry.object_info.raw.image_addr.0);

        // back-fill the `image_size` in case it is missing (or 0), so that it spans up to the
        // next image.
        // This is clearly defined in the docs at https://develop.sentry.dev/sdk/event-payloads/debugmeta/#debug-images,
        // which also explicitly state that this "might lead to invalid stack traces".
        // As this is exclusively used with `unwrap_or(0)`, there is no difference between
        // `None` and `Some(0)`.
        // In reality though, the last module in the list is the only one that can have `None`.
        // By definition, if the last module has a `0` size, it extends to infinity.
        if modules.len() > 1 {
            for i in 0..modules.len() - 1 {
                let next_addr = modules
                    .get(i + 1)
                    .map(|entry| entry.object_info.raw.image_addr.0);
                if let Some(entry) = modules.get_mut(i) {
                    if entry.object_info.raw.image_size.unwrap_or(0) == 0 {
                        let entry_addr = entry.object_info.raw.image_addr.0;
                        let size = next_addr.unwrap_or(entry_addr) - entry_addr;
                        entry.object_info.raw.image_size = Some(size);
                    }
                }
            }
        }

        Self {
            modules,
            scope,
            sources,
        }
    }

    /// Returns the original `CompleteObjectInfo` list in its original sorting order.
    pub fn into_inner(mut self) -> Vec<CompleteObjectInfo> {
        self.modules.sort_by_key(|entry| entry.module_index);
        self.modules
            .into_iter()
            .map(|entry| entry.object_info)
            .collect()
    }

    /// Fetches all the SymCaches for the modules referenced by the `stacktraces`.
    #[tracing::instrument(skip_all)]
    pub async fn fetch_caches(
        &mut self,
        symcache_actor: SymCacheActor,
        ppdb_cache_actor: PortablePdbCacheActor,
        stacktraces: &[RawStacktrace],
    ) {
        let mut referenced_objects = HashSet::new();
        for stacktrace in stacktraces {
            for frame in &stacktrace.frames {
                if let Some(CacheLookupResult { module_index, .. }) =
                    self.lookup_cache(frame.instruction_addr.0, frame.addr_mode)
                {
                    referenced_objects.insert(module_index);
                }
            }
        }

        let futures = self
            .modules
            .iter_mut()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let is_used = referenced_objects.contains(&entry.module_index);
                if !is_used {
                    entry.object_info.debug_status = ObjectFileStatus::Unused;
                    return None;
                }
                let symcache_actor = symcache_actor.clone();
                let ppdb_cache_actor = ppdb_cache_actor.clone();
                let identifier = object_id_from_object_info(&entry.object_info.raw);
                let sources = self.sources.clone();
                let scope = self.scope.clone();
                let object_type = entry.object_info.raw.ty;

                let fut = async move {
                    match object_type {
                        ObjectType::PeDotnet => {
                            let request = FetchPortablePdbCache {
                                identifier,
                                sources,
                                scope,
                            };

                            let DerivedCache {
                                cache,
                                candidates,
                                features,
                            } = ppdb_cache_actor.fetch(request).await;

                            (
                                idx,
                                CacheFile {
                                    file: cache.map(CacheFileEntry::PortablePdbCache),
                                    candidates,
                                    features,
                                },
                            )
                        }
                        _ => {
                            let request = FetchSymCache {
                                object_type,
                                identifier,
                                sources,
                                scope,
                            };

                            let DerivedCache {
                                cache,
                                candidates,
                                features,
                            } = symcache_actor.fetch(request).await;

                            (
                                idx,
                                CacheFile {
                                    file: cache.map(CacheFileEntry::SymCache),
                                    candidates,
                                    features,
                                },
                            )
                        }
                    }
                }
                .bind_hub(Hub::new_from_top(Hub::current()));
                Some(fut)
            });

        for (
            idx,
            CacheFile {
                file,
                candidates,
                features,
            },
        ) in future::join_all(futures).await
        {
            if let Some(entry) = self.modules.get_mut(idx) {
                entry.object_info.arch = Default::default();
                entry.object_info.features.merge(features);
                entry.object_info.candidates.merge(&candidates);
                entry.object_info.debug_status = object_file_status_from_cache_entry(&file);

                if let Ok(CacheFileEntry::SymCache(ref symcache)) = file {
                    entry.object_info.arch = symcache.get().arch();
                }

                entry.cache = file;
            }
        }
    }

    /// Fetches all the sources for the modules referenced by the `stacktraces`.
    #[tracing::instrument(skip_all)]
    pub async fn fetch_sources(
        &mut self,
        objects: ObjectsActor,
        stacktraces: &[CompleteStacktrace],
    ) {
        let mut referenced_objects = HashSet::new();
        for stacktrace in stacktraces {
            for frame in &stacktrace.frames {
                if let Some(entry) =
                    self.get_module_by_addr(frame.raw.instruction_addr.0, frame.raw.addr_mode)
                {
                    referenced_objects.insert(entry.module_index);
                }
            }
        }

        let futures = self
            .modules
            .iter_mut()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let is_used = referenced_objects.contains(&entry.module_index);
                if !is_used {
                    entry.object_info.debug_status = ObjectFileStatus::Unused;
                    entry.source_object = Err(CacheError::NotFound);
                    return None;
                }

                let objects = objects.clone();
                let find_request = FindObject {
                    filetypes: FileType::sources(),
                    purpose: ObjectPurpose::Source,
                    identifier: object_id_from_object_info(&entry.object_info.raw),
                    sources: self.sources.clone(),
                    scope: self.scope.clone(),
                };

                Some(
                    async move {
                        let FindResult { meta, candidates } = objects.find(find_request).await;

                        dbg!(&meta, &candidates);

                        let source_object = match meta {
                            Some(meta) => match meta.handle {
                                Ok(handle) => objects.fetch(handle).await,
                                Err(err) => Err(err),
                            },
                            None => Err(CacheError::NotFound),
                        };

                        (idx, source_object, candidates)
                    }
                    .bind_hub(Hub::new_from_top(Hub::current())),
                )
            });

        for (idx, source_object, candidates) in future::join_all(futures).await {
            if let Some(entry) = self.modules.get_mut(idx) {
                entry.source_object = source_object;
                entry.object_info.candidates.merge(&candidates);

                if entry.source_object.is_ok() {
                    entry.object_info.features.has_sources = true;
                }
            }
        }
    }

    /// Look up the corresponding SymCache based on the instruction `addr`.
    pub fn lookup_cache(&self, addr: u64, addr_mode: AddrMode) -> Option<CacheLookupResult<'_>> {
        self.get_module_by_addr(addr, addr_mode).map(|entry| {
            let relative_addr = match addr_mode {
                AddrMode::Abs => entry.object_info.abs_to_rel_addr(addr),
                AddrMode::Rel(_) => Some(addr),
            };
            CacheLookupResult {
                module_index: entry.module_index,
                object_info: &entry.object_info,
                cache: &entry.cache,
                relative_addr,
            }
        })
    }

    /// Creates a [`ObjectDebugSession`] for each module that has a [`SourceObject`].
    ///
    /// This returns a separate HashMap purely to avoid self-referential borrowing issues.
    /// The [`ObjectDebugSession`] borrows from the [`SourceObject`] and thus they can't live within
    /// the same mutable [`ModuleLookup`].
    pub fn prepare_debug_sessions(&self) -> HashMap<usize, Option<ObjectDebugSession<'_>>> {
        self.modules
            .iter()
            .map(|entry| {
                // FIXME(swatinem): we should log these errors here
                let debug_session = entry
                    .source_object
                    .as_ref()
                    .ok()
                    .and_then(|o| o.object().debug_session().ok());

                (entry.module_index, debug_session)
            })
            .collect()
    }

    /// This looks up the source of the given line, plus `n` lines above/below.
    pub fn get_context_lines(
        &self,
        debug_sessions: &HashMap<usize, Option<ObjectDebugSession<'_>>>,
        addr: u64,
        addr_mode: AddrMode,
        abs_path: &str,
        lineno: u32,
        n: usize,
    ) -> Option<(Vec<String>, String, Vec<String>)> {
        let entry = self.get_module_by_addr(addr, addr_mode)?;
        let session = debug_sessions.get(&entry.module_index)?.as_ref()?;
        let source = session.source_by_path(abs_path).ok()??;

        let lineno = lineno as usize;
        let start_line = lineno.saturating_sub(n);
        let line_diff = lineno - start_line;

        let mut lines = source.lines().skip(start_line);
        let pre_context = (&mut lines)
            .take(line_diff.saturating_sub(1))
            .map(|x| x.to_string())
            .collect();
        let context = lines.next()?.to_string();
        let post_context = lines.take(n).map(|x| x.to_string()).collect();

        Some((pre_context, context, post_context))
    }

    /// Looks up the [`ModuleEntry`] for the given `addr` and `addr_mode`.
    fn get_module_by_addr(&self, addr: u64, addr_mode: AddrMode) -> Option<&ModuleEntry> {
        match addr_mode {
            AddrMode::Abs => {
                let idx = match self
                    .modules
                    .binary_search_by_key(&addr, |entry| entry.object_info.raw.image_addr.0)
                {
                    Ok(idx) => idx,
                    Err(idx) if idx == 0 => {
                        return None;
                    }
                    Err(idx) => idx - 1,
                };
                let entry = self.modules.get(idx)?;

                let start_addr = entry.object_info.raw.image_addr.0;
                let size = entry.object_info.raw.image_size.unwrap_or(0);
                let end_addr = start_addr.checked_add(size)?;

                if end_addr < addr && size != 0 {
                    // The debug image ends at a too low address and we're also confident that
                    // end_addr is accurate (size != 0)
                    return None;
                }

                Some(entry)
            }
            AddrMode::Rel(this_module_index) => self
                .modules
                .iter()
                .find(|entry| entry.module_index == this_module_index),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::RawObjectInfo;
    use crate::utils::hex::HexValue;

    use super::*;

    #[test]
    fn backfill_image_size() {
        let raw_modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[{
                "type":"unknown",
                "image_addr": "0x2000",
                "image_size": 4096
            },{
                "type":"unknown",
                "image_addr": "0x1000"
            },{
                "type":"unknown",
                "image_addr": "0x3000"
            }]"#,
        )
        .unwrap();

        let modules = ModuleLookup::new(
            Scope::Global,
            Arc::new([]),
            raw_modules.into_iter().map(From::from),
        );

        let modules = modules.into_inner();

        assert_eq!(modules[0].raw.image_addr.0, 8192);
        assert_eq!(modules[0].raw.image_size, Some(4096));
        assert_eq!(modules[1].raw.image_addr.0, 4096);
        assert_eq!(modules[1].raw.image_size, Some(4096));
        assert_eq!(modules[2].raw.image_addr.0, 12288);
        assert_eq!(modules[2].raw.image_size, None);
    }

    #[test]
    fn module_lookup() {
        let raw_modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[{
                "code_id": "b",
                "type":"unknown",
                "image_addr": "0x2000",
                "image_size": 4096
            },{
                "code_id": "a",
                "type":"unknown",
                "image_addr": "0x1000"
            },{
                "code_id": "c",
                "type":"unknown",
                "image_addr": "0x4000"
            }]"#,
        )
        .unwrap();

        let modules = ModuleLookup::new(
            Scope::Global,
            Arc::new([]),
            raw_modules.into_iter().map(From::from),
        );

        let entry = modules.lookup_cache(0x1234, AddrMode::Abs);
        assert_eq!(entry.unwrap().object_info.raw.code_id.as_deref(), Some("a"));

        let entry = modules.lookup_cache(0x3456, AddrMode::Abs);
        assert!(entry.is_none());

        let entry = modules.lookup_cache(0x3456, AddrMode::Rel(0));
        assert_eq!(entry.unwrap().object_info.raw.code_id.as_deref(), Some("b"));

        let entry = modules.lookup_cache(0x4567, AddrMode::Abs);
        assert_eq!(entry.unwrap().object_info.raw.code_id.as_deref(), Some("c"));
    }

    #[test]
    fn test_symcache_lookup_open_end_addr() {
        // The Rust SDK and some other clients sometimes send zero-sized images when no end addr
        // could be determined. Symbolicator should still resolve such images.
        let info = CompleteObjectInfo::from(RawObjectInfo {
            ty: ObjectType::Unknown,
            code_id: None,
            debug_id: None,
            code_file: None,
            debug_file: None,
            image_addr: HexValue(42),
            image_size: Some(0),
            checksum: None,
        });

        let lookup = ModuleLookup::new(Scope::Global, Arc::new([]), std::iter::once(info.clone()));

        let lookup_result = lookup.lookup_cache(43, AddrMode::Abs).unwrap();
        assert_eq!(lookup_result.module_index, 0);
        assert_eq!(lookup_result.object_info, &info);
        assert!(matches!(lookup_result.cache, Err(CacheError::NotFound)));
    }
}
