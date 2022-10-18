use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future;
use sentry::{Hub, SentryFutureExt};
use symbolic::common::{ByteView, SelfCell};
use symbolic::debuginfo::{Object, ObjectDebugSession};
use thiserror::Error;

use crate::services::objects::{FindObject, FoundObject, ObjectPurpose, ObjectsActor};
use crate::services::ppdb_caches::{
    FetchPortablePdbCache, PortablePdbCacheActor, PortablePdbCacheError, PortablePdbCacheFile,
};
use crate::services::symcaches::{FetchSymCache, SymCacheActor, SymCacheError, SymCacheFile};
use crate::sources::{FileType, SourceConfig};
use crate::types::{
    CompleteObjectInfo, CompleteStacktrace, ObjectFileStatus, ObjectType, RawStacktrace, Scope,
};
use crate::utils::addr::AddrMode;

use super::object_id_from_object_info;

#[derive(Debug, Error)]
pub enum CacheFileError {
    #[error(transparent)]
    SymCache(#[from] Arc<SymCacheError>),
    #[error(transparent)]
    PortablePdbCache(#[from] Arc<PortablePdbCacheError>),
}

#[derive(Debug, Clone)]
pub enum CacheFile {
    SymCache(Arc<SymCacheFile>),
    PortablePdbCache(Arc<PortablePdbCacheFile>),
}

#[derive(Debug, Clone)]
pub struct CacheLookupResult<'a> {
    pub module_index: usize,
    pub object_info: &'a CompleteObjectInfo,
    pub cache: Option<&'a CacheFile>,
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

pub struct SourceObject(SelfCell<ByteView<'static>, Object<'static>>);

struct ModuleEntry {
    module_index: usize,
    object_info: CompleteObjectInfo,
    cache: Option<CacheFile>,
    source_object: Option<SourceObject>,
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
                cache: None,
                source_object: None,
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

                Some(
                    async move {
                        match object_type {
                            ObjectType::PeDotnet => {
                                let request = FetchPortablePdbCache {
                                    identifier,
                                    sources,
                                    scope,
                                };

                                let ppdb_cache_result = match ppdb_cache_actor.fetch(request).await
                                {
                                    Ok(ppdb_cache) => Ok(CacheFile::PortablePdbCache(ppdb_cache)),
                                    Err(e) => Err(e.as_ref().into()),
                                };

                                (idx, ppdb_cache_result)
                            }
                            _ => {
                                let request = FetchSymCache {
                                    object_type,
                                    identifier,
                                    sources,
                                    scope,
                                };

                                let symcache_result = match symcache_actor.fetch(request).await {
                                    Ok(symcache) => Ok(CacheFile::SymCache(symcache)),
                                    Err(e) => Err(e.as_ref().into()),
                                };

                                (idx, symcache_result)
                            }
                        }
                    }
                    .bind_hub(Hub::new_from_top(Hub::current())),
                )
            });

        for (idx, cache_result) in future::join_all(futures).await {
            if let Some(entry) = self.modules.get_mut(idx) {
                let (cache, status) = match cache_result {
                    Ok(cache) => match cache {
                        CacheFile::SymCache(ref symcache) => match symcache.parse() {
                            Ok(Some(_)) => (Some(cache), ObjectFileStatus::Found),
                            Ok(None) => (Some(cache), ObjectFileStatus::Missing),
                            Err(e) => (None, (&e).into()),
                        },

                        CacheFile::PortablePdbCache(ref ppdb_cache) => match ppdb_cache.parse() {
                            Ok(Some(_)) => (Some(cache), ObjectFileStatus::Found),
                            Ok(None) => (Some(cache), ObjectFileStatus::Missing),
                            Err(e) => (None, (&e).into()),
                        },
                    },
                    Err(e) => (None, e),
                };

                entry.object_info.arch = Default::default();
                entry.object_info.debug_status = status;

                match cache {
                    Some(CacheFile::SymCache(ref symcache)) => {
                        entry.object_info.arch = symcache.arch();
                        entry.object_info.features.merge(symcache.features());
                        entry.object_info.candidates.merge(symcache.candidates());
                    }

                    Some(CacheFile::PortablePdbCache(ref ppdb_cache)) => {
                        entry.object_info.features.merge(ppdb_cache.features());
                        entry.object_info.candidates.merge(ppdb_cache.candidates());
                    }

                    None => {}
                }

                entry.cache = cache;
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
                    entry.source_object = None;
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
                        let FoundObject { meta, candidates } =
                            objects.find(find_request).await.unwrap_or_default();

                        let source_object = match meta {
                            None => None,
                            Some(object_file_meta) => {
                                objects.fetch(object_file_meta).await.ok().and_then(|x| {
                                    SelfCell::try_new(x.data(), |b| Object::parse(unsafe { &*b }))
                                        .map(SourceObject)
                                        .ok()
                                })
                            }
                        };

                        (idx, source_object, candidates)
                    }
                    .bind_hub(Hub::new_from_top(Hub::current())),
                )
            });

        for (idx, source_object, candidates) in future::join_all(futures).await {
            if let Some(entry) = self.modules.get_mut(idx) {
                entry.source_object = source_object;

                if entry.source_object.is_some() {
                    entry.object_info.features.has_sources = true;
                    entry.object_info.candidates.merge(&candidates);
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
                cache: entry.cache.as_ref(),
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
                (
                    entry.module_index,
                    entry
                        .source_object
                        .as_ref()
                        .and_then(|o| o.0.get().debug_session().ok()),
                )
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

        let entry = modules.get_module_by_addr(0x1234, AddrMode::Abs);
        assert_eq!(entry.unwrap().object_info.raw.code_id.as_deref(), Some("a"));

        let entry = modules.get_module_by_addr(0x3456, AddrMode::Abs);
        assert!(entry.is_none());

        let entry = modules.get_module_by_addr(0x3456, AddrMode::Rel(0));
        assert_eq!(entry.unwrap().object_info.raw.code_id.as_deref(), Some("b"));

        let entry = modules.get_module_by_addr(0x4567, AddrMode::Abs);
        assert_eq!(entry.unwrap().object_info.raw.code_id.as_deref(), Some("c"));
    }
}
