use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use async_trait::async_trait;
use minidump::MinidumpContext;
use minidump::{MinidumpModule, Module};
use minidump_processor::{
    FillSymbolError, FrameSymbolizer, FrameWalker, SymbolFile, SymbolProvider, SymbolStats,
};
use parking_lot::RwLock;
use sentry::types::DebugId;
use sentry::SentryFutureExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use symbolic::common::ByteView;
use symbolic_minidump::cfi::CfiCache;
use tempfile::TempPath;

use crate::services::minidump::parse_stacktraces_from_minidump;
use crate::sources::SourceConfig;
use crate::types::{
    CompleteObjectInfo, FrameTrust, ObjectFileStatus, ObjectType, RawFrame, RawObjectInfo,
    RawStacktrace, Registers, RequestOptions, Scope,
};
use crate::utils::futures::{m, measure};
use crate::utils::hex::HexValue;

use super::{
    CfiCacheModules, Minidump, MinidumpState, StacktraceOrigin, SymbolicateStacktraces,
    SymbolicationActor, SymbolicationError,
};

/// Generic error serialized over procspawn.
#[derive(Debug, Serialize, Deserialize)]
struct ProcError(String);

impl ProcError {
    pub fn new(d: impl std::fmt::Display) -> Self {
        Self(d.to_string())
    }
}

impl From<std::io::Error> for ProcError {
    fn from(e: std::io::Error) -> Self {
        Self::new(e)
    }
}
impl From<minidump::Error> for ProcError {
    fn from(e: minidump::Error) -> Self {
        Self::new(e)
    }
}

impl From<minidump_processor::ProcessError> for ProcError {
    fn from(e: minidump_processor::ProcessError) -> Self {
        Self::new(e)
    }
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ProcError {}

#[derive(Debug, Serialize, Deserialize)]
struct StackWalkMinidumpResult {
    modules: Option<Vec<(DebugId, RawObjectInfo)>>,
    missing_modules: Vec<DebugId>,
    stacktraces: Vec<RawStacktrace>,
    minidump_state: MinidumpState,
    duration: std::time::Duration,
}

struct SymbolicatorSymbolProvider {
    files: BTreeMap<DebugId, SymbolFile>,
    missing_ids: RwLock<BTreeSet<DebugId>>,
}

impl SymbolicatorSymbolProvider {
    /// Load the CFI information from the cache.
    ///
    /// This reads the CFI caches from disk and returns them in a format suitable for the
    /// breakpad processor to stackwalk.
    pub fn new<'a, M: Iterator<Item = &'a (DebugId, PathBuf)>>(modules: M) -> Self {
        // TODO(ja): Make TempSymbolProvider the thing serialized to procspawn (prepares for moving in-process)
        Self {
            files: modules
                .filter_map(|(id, path)| Some((*id, Self::load(path)?)))
                .collect(),

            missing_ids: RwLock::new(BTreeSet::new()),
        }
    }

    fn load(cfi_path: &Path) -> Option<SymbolFile> {
        sentry::with_scope(
            |scope| {
                scope.set_extra(
                    "cfi_cache",
                    sentry::protocol::Value::String(cfi_path.to_string_lossy().to_string()),
                )
            },
            || {
                let bytes = ByteView::open(cfi_path)
                    .map_err(|err| {
                        let stderr: &dyn std::error::Error = &err;
                        tracing::error!(stderr, "Error while reading cficache");
                    })
                    .ok()?;

                let cfi_cache = CfiCache::from_bytes(bytes)
                    // This mostly never happens since we already checked the files
                    // after downloading and they would have been tagged with
                    // CacheStatus::Malformed.
                    .map_err(|err| {
                        let stderr: &dyn std::error::Error = &err;
                        tracing::error!(stderr, "Error while loading cficache");
                    })
                    .ok()?;

                if cfi_cache.as_slice().is_empty() {
                    return None;
                }

                SymbolFile::from_bytes(cfi_cache.as_slice())
                    .map_err(|err| {
                        let stderr: &dyn std::error::Error = &err;
                        tracing::error!(stderr, "Error while processing cficache");
                    })
                    .ok()
            },
        )
    }

    fn missing_ids(self) -> Vec<DebugId> {
        self.missing_ids.into_inner().into_iter().collect()
    }
}

#[async_trait]
impl SymbolProvider for SymbolicatorSymbolProvider {
    async fn fill_symbol(
        &self,
        module: &(dyn Module + Sync),
        _frame: &mut (dyn FrameSymbolizer + Send),
    ) -> Result<(), FillSymbolError> {
        let debug_id = module.debug_identifier().ok_or(FillSymbolError {})?;

        // Symbolicator's CFI caches never store symbolication information. However, we could hook
        // up symbolic here to fill frame info right away. This requires a larger refactor of
        // minidump processing and the types, however.
        // TODO(ja): Check if this is OK. Shouldn't trigger skip heuristics
        match self.files.contains_key(&debug_id) {
            true => Ok(()),
            false => Err(FillSymbolError {}),
        }
    }

    async fn walk_frame(
        &self,
        module: &(dyn Module + Sync),
        walker: &mut (dyn FrameWalker + Send),
    ) -> Option<()> {
        let debug_id = module.debug_identifier()?;
        match self.files.get(&debug_id) {
            Some(file) => file.walk_frame(module, walker),
            None => {
                self.missing_ids.write().insert(debug_id);
                None
            }
        }
    }

    fn stats(&self) -> HashMap<String, SymbolStats> {
        self.files
            .iter()
            .map(|(debug_id, sym)| {
                let stats = SymbolStats {
                    symbol_url: sym.url.clone(), // TODO(ja): We could put our candidate URI here
                    loaded_symbols: true, // TODO(ja): Should we return `false` for not found?
                    corrupt_symbols: false,
                };

                (debug_id.to_string(), stats)
            })
            .collect()
    }
}

fn object_info_from_minidump_module(ty: ObjectType, module: &MinidumpModule) -> RawObjectInfo {
    // Some modules are not objects but rather fonts or JIT areas or other mmapped files
    // which we don't care about.  These may not have complete information so map these to
    // our schema by converting to None when needed.
    let code_id = module
        .code_identifier()
        .filter(|code_id| !code_id.is_nil())
        .map(|code_id| code_id.to_string().to_lowercase());
    let code_file = module.code_file();
    let code_file = match code_file.is_empty() {
        true => None,
        false => Some(code_file.into_owned()),
    };

    RawObjectInfo {
        ty,
        code_id,
        code_file,
        debug_id: module.debug_identifier().map(|c| c.breakpad().to_string()),
        debug_file: module.debug_file().map(|c| c.into_owned()),
        image_addr: HexValue(module.base_address()),
        image_size: match module.size() {
            0 => None,
            size => Some(size),
        },
    }
}

async fn stackwalk_with_rust_minidump(
    cfi_caches: Vec<(DebugId, PathBuf)>,
    minidump_path: PathBuf,
    spawn_time: SystemTime,
    return_modules: bool,
) -> Result<StackWalkMinidumpResult, ProcError> {
    if let Ok(duration) = spawn_time.elapsed() {
        metric!(timer("minidump.stackwalk.spawn.duration") = duration);
    }

    // Stackwalk the minidump.
    let duration = Instant::now();
    let minidump = Minidump::read(ByteView::open(minidump_path)?)?;
    let provider = SymbolicatorSymbolProvider::new(cfi_caches.iter());
    let process_state = minidump_processor::process_minidump(&minidump, &provider).await?;
    let duration = duration.elapsed();

    let minidump_state = MinidumpState::from_process_state(&process_state);
    let object_type = minidump_state.object_type();

    let missing_modules = provider.missing_ids();
    let modules = return_modules.then(|| {
        process_state
            .modules
            .iter()
            .map(|module| {
                (
                    // TODO(ja): Check how this can be empty and how we shim.
                    //           Probably needs explicit conversion from raw
                    module.debug_identifier().unwrap_or_default(),
                    object_info_from_minidump_module(object_type, module),
                )
            })
            .collect()
    });

    // Finally iterate through the threads and build the stacktraces to
    // return, marking modules as used when they are referenced by a frame.
    let requesting_thread_index: Option<usize> = process_state.requesting_thread;
    let threads = process_state.threads;
    let mut stacktraces = Vec::with_capacity(threads.len());
    for (index, thread) in threads.iter().enumerate() {
        let registers = match thread.frames.get(0) {
            Some(frame) => map_symbolic_registers(&frame.context),
            None => Registers::new(),
        };

        // Trim infinite recursions explicitly because those do not
        // correlate to minidump size. Every other kind of bloated
        // input data we know is already trimmed/rejected by raw
        // byte size alone.
        let frame_count = thread.frames.len().min(20000);
        let mut frames = Vec::with_capacity(frame_count);
        for frame in thread.frames.iter().take(frame_count) {
            frames.push(RawFrame {
                instruction_addr: HexValue(frame.resume_address),
                package: frame.module.as_ref().map(|m| m.code_file().into_owned()),
                trust: frame.trust.into(),
                ..RawFrame::default()
            });
        }

        stacktraces.push(RawStacktrace {
            is_requesting: requesting_thread_index.map(|r| r == index),
            thread_id: Some(thread.thread_id.into()),
            registers,
            frames,
        });
    }

    Ok(StackWalkMinidumpResult {
        modules,
        missing_modules,
        stacktraces,
        minidump_state,
        duration,
    })
}

/// Builder object to collect modules from the minidump.
///
/// This collects the modules found by the minidump processor and constructs the
/// [`CompleteObjectInfo`] objects from them, which are used to eventually return the
/// module list in the [`SymbolicateStacktraces`] object of the final JSON API response.
///
/// The builder requires marking modules that are referenced by stack frames.  This allows it to
/// omit modules which do not look like real modules (i.e. modules which don't have a valid
/// code or debug ID, they could be mmap'ed fonts or other such things) as long as they are
/// not mapped to address ranges used by any frames in the stacktraces.
struct ModuleListBuilder {
    inner: Vec<(CompleteObjectInfo, bool)>,
}

impl ModuleListBuilder {
    fn new(cfi_caches: CfiCacheModules, modules: Vec<(DebugId, RawObjectInfo)>) -> Self {
        // Now build the CompletedObjectInfo for all modules
        let cfi_caches = cfi_caches.into_inner();

        let mut inner: Vec<(CompleteObjectInfo, bool)> = modules
            .into_iter()
            .map(|(id, raw_info)| {
                let mut obj_info: CompleteObjectInfo = raw_info.into();

                // If we loaded this module into the CFI cache, update the info object with
                // this status.
                match cfi_caches.get(&id) {
                    None => {
                        obj_info.unwind_status = None;
                    }
                    Some(cfi_module) => {
                        obj_info.unwind_status = Some(cfi_module.cfi_status);
                        obj_info.features.merge(cfi_module.features);
                        obj_info.candidates = cfi_module.cfi_candidates.clone();
                    }
                }
                (obj_info, false)
            })
            .collect();

        // Sort by image address for binary search in `mark`.
        inner.sort_by_key(|(info, _)| info.raw.image_addr);
        Self { inner }
    }

    /// Finds the index of the module (for lookup in `self.modules`) that covers the gives `addr`.
    fn find_module_index(&self, addr: u64) -> Option<usize> {
        let search_index = self
            .inner
            .binary_search_by_key(&addr, |(info, _)| info.raw.image_addr.0);

        let info_idx = match search_index {
            Ok(index) => index,
            Err(0) => return None,
            Err(index) => index - 1,
        };

        let (info, _marked) = &self.inner[info_idx];
        let HexValue(image_addr) = info.raw.image_addr;
        let includes_addr = match info.raw.image_size {
            Some(size) => addr < image_addr + size,
            // If there is no image size, the image implicitly counts up to the next image. Because
            // we know that the search address is somewhere in this range, we can mark it.
            None => true,
        };

        if includes_addr {
            Some(info_idx)
        } else {
            None
        }
    }

    /// Walks all the `stacktraces`, marking modules as being referenced based on the frames addr.
    fn process_stacktraces(&mut self, stacktraces: &[RawStacktrace]) {
        for trace in stacktraces {
            for frame in &trace.frames {
                let addr = frame.instruction_addr.0;
                let is_prewalked = frame.trust == FrameTrust::PreWalked;
                self.mark_referenced(addr, is_prewalked);
            }
        }
    }

    /// Marks the module loaded at the given address as referenced.
    ///
    /// The respective module will always be included in the final list of modules.
    pub fn mark_referenced(&mut self, addr: u64, is_prewalked: bool) {
        let info_index = match self.find_module_index(addr) {
            Some(idx) => idx,
            None => return,
        };

        let (info, marked) = &mut self.inner[info_index];
        *marked = true;

        if info.unwind_status.is_none() && !is_prewalked {
            info.unwind_status = Some(ObjectFileStatus::Missing);
        }
    }

    /// Returns the modules list to be used in the symbolication response.
    pub fn build(self) -> Vec<CompleteObjectInfo> {
        self.inner
            .into_iter()
            .filter_map(|(mut info, marked)| {
                let include = marked || info.raw.debug_id.is_some();
                if !include {
                    return None;
                }
                // Reset the unwind status to `unused` for all objects that were not being referenced
                // in the final stack traces.
                if !marked || info.unwind_status.is_none() {
                    info.unwind_status = Some(ObjectFileStatus::Unused);
                }
                metric!(
                    counter("symbolication.unwind_status") += 1,
                    "status" => info.unwind_status.unwrap_or(ObjectFileStatus::Unused).name(),
                );
                Some(info)
            })
            .collect()
    }
}

impl SymbolicationActor {
    /// Join a procspawn handle with a timeout.
    ///
    /// This handles the procspawn result, makes sure to appropriately log any failures and
    /// save the minidump for debugging.  Returns a simple result converted to the
    /// [`SymbolicationError`].
    fn join_procspawn<T, E>(
        handle: procspawn::JoinHandle<Result<procspawn::serde::Json<T>, E>>,
        timeout: Duration,
        metric: &str,
    ) -> Result<T, anyhow::Error>
    where
        T: Serialize + DeserializeOwned,
        E: Into<anyhow::Error> + Serialize + DeserializeOwned,
    {
        match handle.join_timeout(timeout) {
            Ok(Ok(procspawn::serde::Json(out))) => Ok(out),
            Ok(Err(err)) => Err(err.into()),
            Err(perr) => {
                let reason = if perr.is_timeout() {
                    "timeout"
                } else if perr.is_panic() {
                    "panic"
                } else if perr.is_remote_close() {
                    "remote-close"
                } else if perr.is_cancellation() {
                    "canceled"
                } else {
                    "unknown"
                };
                metric!(counter(metric) += 1, "reason" => reason);
                Err(anyhow::Error::new(perr))
            }
        }
    }

    /// Unwind the stack from a minidump.
    ///
    /// This processes the minidump to stackwalk all the threads found in the minidump.
    ///
    /// The `cfi_results` will contain all modules found in the minidump and the result of trying
    /// to fetch the Call Frame Information (CFI) for them from the [`CfiCacheActor`].
    ///
    /// This function will load the CFI files and ask breakpad to stackwalk the minidump.
    /// Once it has stacktraces it creates the list of used modules and returns the
    /// un-symbolicated stacktraces in a structure suitable for requesting symbolication.
    ///
    /// The module list returned is usable for symbolication itself and is also directly
    /// used in the final symbolication response of the public API.  It will contain all
    /// modules which either have been referenced by any of the frames in the stacktraces or
    /// have a full debug id.  This is intended to skip over modules like `mmap`ed fonts or
    /// similar which are mapped in the address space but do not actually contain executable
    /// modules.
    #[tracing::instrument(skip_all)]
    async fn stackwalk_minidump(
        &self,
        path: &Path,
        cfi_caches: &CfiCacheModules,
        return_modules: bool,
    ) -> anyhow::Result<StackWalkMinidumpResult> {
        let pool = self.spawnpool.clone();
        let cfi_caches = cfi_caches.for_processing();
        let minidump_path = path.to_path_buf();

        let lazy = async move {
            let spawn_time = std::time::SystemTime::now();
            let spawn_result = pool.spawn(
                (
                    procspawn::serde::Json(cfi_caches.clone()),
                    minidump_path.clone(),
                    spawn_time,
                    return_modules,
                ),
                |(cfi_caches, minidump_path, spawn_time, return_modules)| -> Result<_, ProcError> {
                    let procspawn::serde::Json(cfi_caches) = cfi_caches;
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        stackwalk_with_rust_minidump(
                            cfi_caches,
                            minidump_path,
                            spawn_time,
                            return_modules,
                        )
                        .await
                        .map(procspawn::serde::Json)
                    })
                },
            );

            let result = Self::join_procspawn(
                spawn_result,
                Duration::from_secs(60),
                "minidump.stackwalk.spawn.error",
            )?;

            metric!(timer("minidump.stackwalk.duration") = result.duration);

            Ok::<_, anyhow::Error>(result)
        };

        self.cpu_pool
            .spawn(lazy.bind_hub(sentry::Hub::current()))
            .await
            .context("Minidump stackwalk future cancelled")?
    }

    /// Iteratively stackwalks/processes the given `minidump_file` using breakpad.
    async fn stackwalk_minidump_iteratively(
        &self,
        scope: Scope,
        minidump_path: &Path,
        sources: Arc<[SourceConfig]>,
        cfi_caches: &mut CfiCacheModules,
    ) -> anyhow::Result<StackWalkMinidumpResult> {
        let mut iterations = 0;

        let mut modules: Option<HashMap<DebugId, RawObjectInfo>> = None;

        let mut result = loop {
            iterations += 1;

            let mut result = self
                .stackwalk_minidump(minidump_path, cfi_caches, modules.is_none())
                .await?;

            let modules = match &modules {
                Some(modules) => modules,
                None => {
                    let received_modules = std::mem::take(&mut result.modules);
                    let received_modules =
                        received_modules.unwrap_or_default().into_iter().collect();
                    modules = Some(received_modules);
                    modules.as_ref().expect("modules was just set")
                }
            };

            // We put a hard limit of 5 iterations here.
            // Previously, it was two, once scanning for referenced modules, then doing the stackwalk
            if result.missing_modules.is_empty() || iterations >= 5 {
                break result;
            }

            let missing_modules: Vec<(DebugId, &RawObjectInfo)> =
                std::mem::take(&mut result.missing_modules)
                    .into_iter()
                    .filter_map(|id| modules.get(&id).map(|info| (id, info)))
                    .collect();

            let loaded_caches = self
                .load_cfi_caches(scope.clone(), &missing_modules, sources.clone())
                .await;
            cfi_caches.extend(loaded_caches);
        };

        result.modules = modules.map(|modules| modules.into_iter().collect());
        metric!(time_raw("minidump.stackwalk.iterations") = iterations);
        Ok(result)
    }

    #[tracing::instrument(skip_all)]
    pub(super) async fn do_stackwalk_minidump(
        &self,
        scope: Scope,
        minidump_file: TempPath,
        sources: Arc<[SourceConfig]>,
        options: RequestOptions,
    ) -> Result<(SymbolicateStacktraces, MinidumpState), SymbolicationError> {
        let future = async move {
            let len = minidump_file.metadata()?.len();
            tracing::debug!("Processing minidump ({} bytes)", len);
            metric!(time_raw("minidump.upload.size") = len);

            let mut cfi_caches = CfiCacheModules::new();

            let future = self.stackwalk_minidump_iteratively(
                scope.clone(),
                &minidump_file,
                sources.clone(),
                &mut cfi_caches,
            );

            let result = match future.await {
                Ok(result) => result,
                Err(err) => {
                    self.maybe_persist_minidump(minidump_file);
                    return Err(err);
                }
            };

            let StackWalkMinidumpResult {
                modules,
                mut stacktraces,
                minidump_state,
                ..
            } = result;

            match parse_stacktraces_from_minidump(&ByteView::open(&minidump_file)?) {
                Ok(Some(client_stacktraces)) => merge_clientside_with_processed_stacktraces(
                    &mut stacktraces,
                    client_stacktraces,
                ),
                Err(e) => tracing::error!("invalid minidump extension: {}", e),
                _ => (),
            }

            // Start building the module list for the symbolication response.
            let mut module_builder =
                ModuleListBuilder::new(cfi_caches, modules.unwrap_or_default());
            module_builder.process_stacktraces(&stacktraces);

            let request = SymbolicateStacktraces {
                modules: module_builder.build(),
                scope,
                sources,
                origin: StacktraceOrigin::Minidump,
                signal: None,
                stacktraces,
                options,
            };

            Ok::<_, anyhow::Error>((request, minidump_state))
        };

        let future = tokio::time::timeout(Duration::from_secs(3600), future);
        let future = measure("minidump_stackwalk", m::timed_result, None, future);
        future
            .await
            .map(|ret| ret.map_err(SymbolicationError::from))
            .unwrap_or(Err(SymbolicationError::Timeout))
    }
}

/// Merges the Stack Traces processed via Breakpad with the ones captured on the Client.
///
/// For now, this means we will prefer the client-side stack trace over the processed one, but in
/// the future we could be a bit smarter about what to do.
fn merge_clientside_with_processed_stacktraces(
    processed_stacktraces: &mut [RawStacktrace],
    clientside_stacktraces: Vec<RawStacktrace>,
) {
    let mut client_traces_by_id: HashMap<_, _> = clientside_stacktraces
        .into_iter()
        .filter_map(|trace| trace.thread_id.map(|thread_id| (thread_id, trace)))
        .collect();

    for thread in processed_stacktraces {
        if let Some(thread_id) = thread.thread_id {
            if let Some(client_thread) = client_traces_by_id.remove(&thread_id) {
                // NOTE: we could gather all kinds of metrics here, as in:
                // - are we finding more or less frames via CFI?
                // - how many frames are the same
                // - etc.
                // We could also be a lot smarter about which threads/frames we chose. For now we
                // will just always prefer client-side stack traces
                if !client_thread.frames.is_empty() {
                    thread.frames = client_thread.frames;
                }
            }
        }
    }
}

fn map_symbolic_registers(context: &MinidumpContext) -> BTreeMap<String, HexValue> {
    context
        .valid_registers()
        .map(|(reg, val)| (reg.to_owned(), HexValue(val)))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::sync::Arc;

    use tempfile::NamedTempFile;

    use crate::services::symbolication::stackwalking::ModuleListBuilder;
    use crate::services::symbolication::tests::setup_service;
    use crate::test;
    use crate::types::Scope;
    use crate::types::{
        CompleteObjectInfo, ObjectFileStatus, ObjectType, RawObjectInfo, RequestOptions,
    };
    use crate::utils::hex::HexValue;

    macro_rules! assert_snapshot {
        ($e:expr) => {
            ::insta::assert_yaml_snapshot!($e, {
                ".**.location" => ::insta::dynamic_redaction(
                    $crate::services::symbolication::tests::redact_localhost_port
                )
            });
        }
    }

    fn create_object_info(has_id: bool, addr: u64, size: Option<u64>) -> CompleteObjectInfo {
        let mut info: CompleteObjectInfo = RawObjectInfo {
            ty: ObjectType::Elf,
            code_id: None,
            code_file: None,
            debug_id: Some(uuid::Uuid::new_v4().to_string()).filter(|_| has_id),
            debug_file: None,
            image_addr: HexValue(addr),
            image_size: size,
        }
        .into();
        info.unwind_status = Some(ObjectFileStatus::Unused);
        info
    }

    #[test]
    fn test_code_module_builder_empty() {
        let modules: Vec<CompleteObjectInfo> = vec![];

        let valid = ModuleListBuilder {
            inner: modules.iter().map(|m| (m.clone(), false)).collect(),
        }
        .build();
        assert_eq!(valid, modules);
    }

    #[test]
    fn test_code_module_builder_valid() {
        let modules = vec![
            create_object_info(true, 0x1000, Some(0x1000)),
            create_object_info(true, 0x3000, Some(0x1000)),
        ];

        let valid = ModuleListBuilder {
            inner: modules.iter().map(|m| (m.clone(), false)).collect(),
        }
        .build();
        assert_eq!(valid, modules);
    }

    #[test]
    fn test_code_module_builder_unreferenced() {
        let valid_object = create_object_info(true, 0x1000, Some(0x1000));
        let modules = vec![
            valid_object.clone(),
            create_object_info(false, 0x3000, Some(0x1000)),
        ];

        let valid = ModuleListBuilder {
            inner: modules.iter().map(|m| (m.clone(), false)).collect(),
        }
        .build();
        assert_eq!(valid, vec![valid_object]);
    }

    #[test]
    fn test_code_module_builder_referenced() {
        let modules = vec![
            create_object_info(true, 0x1000, Some(0x1000)),
            create_object_info(false, 0x3000, Some(0x1000)),
        ];

        let mut builder = ModuleListBuilder {
            inner: modules.iter().map(|m| (m.clone(), false)).collect(),
        };
        builder.mark_referenced(0x3500, false);
        let valid = builder.build();
        assert_eq!(valid, modules);
    }

    #[test]
    fn test_code_module_builder_miss_first() {
        let modules = vec![
            create_object_info(false, 0x1000, Some(0x1000)),
            create_object_info(false, 0x3000, Some(0x1000)),
        ];

        let mut builder = ModuleListBuilder {
            inner: modules.iter().map(|m| (m.clone(), false)).collect(),
        };
        builder.mark_referenced(0xfff, false);
        let valid = builder.build();
        assert_eq!(valid, vec![]);
    }

    #[test]
    fn test_code_module_builder_gap() {
        let modules = vec![
            create_object_info(false, 0x1000, Some(0x1000)),
            create_object_info(false, 0x3000, Some(0x1000)),
        ];

        let mut builder = ModuleListBuilder {
            inner: modules.iter().map(|m| (m.clone(), false)).collect(),
        };
        builder.mark_referenced(0x2800, false); // in the gap between both modules
        let valid = builder.build();
        assert_eq!(valid, vec![]);
    }

    #[test]
    fn test_code_module_builder_implicit_size() {
        let valid_object = create_object_info(false, 0x1000, None);
        let modules = vec![
            valid_object.clone(),
            create_object_info(false, 0x3000, None),
        ];

        let mut builder = ModuleListBuilder {
            inner: modules.iter().map(|m| (m.clone(), false)).collect(),
        };
        builder.mark_referenced(0x2800, false); // in the gap between both modules
        let valid = builder.build();
        assert_eq!(valid, vec![valid_object]);
    }

    macro_rules! stackwalk_minidump {
        ($path:expr) => {{
            stackwalk_minidump!(
                $path,
                RequestOptions {
                    dif_candidates: true,
                }
            )
        }};
        ($path:expr, $options:expr) => {{
            async {
                let (service, _cache_dir) = setup_service().await;
                let symbolication = service.symbolication();
                let (_symsrv, source) = test::symbol_server();

                let minidump = test::read_fixture($path);
                let mut minidump_file = NamedTempFile::new()?;
                minidump_file.write_all(&minidump)?;
                let request_id = symbolication.process_minidump(
                    Scope::Global,
                    minidump_file.into_temp_path(),
                    Arc::new([source]),
                    $options,
                );
                let response = symbolication.get_response(request_id.unwrap(), None).await;

                assert_snapshot!(response.unwrap());

                let global_dir = service.config().cache_dir("object_meta/global").unwrap();
                let mut cache_entries: Vec<_> = fs::read_dir(global_dir)?
                    .map(|x| x.unwrap().file_name().into_string().unwrap())
                    .collect();

                cache_entries.sort();
                assert_snapshot!(cache_entries);

                Ok(())
            }
        }};
    }

    #[tokio::test]
    async fn test_minidump_windows() -> anyhow::Result<()> {
        stackwalk_minidump!("windows.dmp").await
    }

    #[tokio::test]
    async fn test_minidump_macos() -> anyhow::Result<()> {
        stackwalk_minidump!("macos.dmp").await
    }

    #[tokio::test]
    async fn test_minidump_linux() -> anyhow::Result<()> {
        stackwalk_minidump!("linux.dmp").await
    }
}
