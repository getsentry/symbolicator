use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::future;
use minidump::system_info::Os;
use minidump::MinidumpContext;
use minidump::{MinidumpModule, Module};
use minidump_processor::{
    FillSymbolError, FrameSymbolizer, FrameWalker, ProcessState, SymbolFile, SymbolProvider,
    SymbolStats,
};
use parking_lot::RwLock;
use sentry::types::DebugId;
use sentry::{Hub, SentryFutureExt};
use serde::{Deserialize, Serialize};
use symbolic::common::{Arch, ByteView};
use symbolic_minidump::cfi::CfiCache;
use tempfile::TempPath;

use crate::cache::CacheStatus;
use crate::services::cficaches::{CfiCacheError, CfiCacheFile, FetchCfiCache};
use crate::services::minidump::parse_stacktraces_from_minidump;
use crate::services::objects::ObjectError;
use crate::sources::SourceConfig;
use crate::types::{
    AllObjectCandidates, CompleteObjectInfo, CompletedSymbolicationResponse, FrameTrust,
    ObjectFeatures, ObjectFileStatus, ObjectType, RawFrame, RawObjectInfo, RawStacktrace,
    Registers, RequestId, RequestOptions, Scope, SystemInfo,
};
use crate::utils::futures::{m, measure};
use crate::utils::hex::HexValue;

use super::{
    object_id_from_object_info, MaxRequestsError, StacktraceOrigin, SymbolicateStacktraces,
    SymbolicationActor, SymbolicationError,
};

type CfiCacheResult = (DebugId, Result<Arc<CfiCacheFile>, Arc<CfiCacheError>>);
type Minidump = minidump::Minidump<'static, ByteView<'static>>;

#[derive(Debug)]
struct StackWalkMinidumpResult {
    cfi_caches: CfiCacheModules,
    modules: Option<Vec<(DebugId, RawObjectInfo)>>,
    missing_modules: Vec<DebugId>,
    stacktraces: Vec<RawStacktrace>,
    minidump_state: MinidumpState,
    duration: std::time::Duration,
}

/// Contains some meta-data about a minidump.
///
/// The minidump meta-data contained here is extracted in the [`stackwalk`]
/// function and merged into the final symbolication result.
///
/// A few more convenience methods exist to help with building the symbolication results.
#[derive(Debug, PartialEq)]
pub(super) struct MinidumpState {
    timestamp: DateTime<Utc>,
    system_info: SystemInfo,
    crashed: bool,
    crash_reason: String,
    assertion: String,
}

impl MinidumpState {
    fn from_process_state(process_state: &ProcessState) -> Self {
        let info = &process_state.system_info;

        let cpu_arch = match info.cpu {
            minidump::system_info::Cpu::X86 => Arch::X86,
            minidump::system_info::Cpu::X86_64 => Arch::Amd64,
            minidump::system_info::Cpu::Ppc => Arch::Ppc,
            minidump::system_info::Cpu::Ppc64 => Arch::Ppc64,
            minidump::system_info::Cpu::Arm => Arch::Arm,
            minidump::system_info::Cpu::Arm64 => Arch::Arm64,
            minidump::system_info::Cpu::Mips => Arch::Mips,
            minidump::system_info::Cpu::Mips64 => Arch::Mips64,
            arch => {
                let msg = format!("Unknown minidump arch: {}", arch);
                sentry::capture_message(&msg, sentry::Level::Error);
                Arch::Unknown
            }
        };

        MinidumpState {
            timestamp: process_state.time.into(),
            system_info: SystemInfo {
                os_name: normalize_minidump_os_name(info.os).to_owned(),
                os_version: info.os_version.clone().unwrap_or_default(),
                os_build: info.os_build.clone().unwrap_or_default(),
                cpu_arch,
                device_model: String::default(),
            },
            crashed: process_state.crashed(),
            crash_reason: process_state
                .crash_reason
                .map(|reason| {
                    let mut reason = reason.to_string();
                    if let Some(addr) = process_state.crash_address {
                        let _ = write!(&mut reason, " / {:#x}", addr);
                    }
                    reason
                })
                .unwrap_or_default(),
            assertion: process_state.assertion.clone().unwrap_or_default(),
        }
    }

    /// Merges this meta-data into a symbolication result.
    ///
    /// This updates the `response` with the meta-data contained.
    fn merge_into(mut self, response: &mut CompletedSymbolicationResponse) {
        if self.system_info.cpu_arch == Arch::Unknown {
            self.system_info.cpu_arch = response
                .modules
                .iter()
                .map(|object| object.arch)
                .find(|arch| *arch != Arch::Unknown)
                .unwrap_or_default();
        }

        response.timestamp = Some(self.timestamp);
        response.system_info = Some(self.system_info);
        response.crashed = Some(self.crashed);
        response.crash_reason = Some(self.crash_reason);
        response.assertion = Some(self.assertion);
    }

    /// Returns the type of executable object that produced this minidump.
    fn object_type(&self) -> ObjectType {
        // Note that the names matched here are normalised by normalize_minidump_os_name().
        match self.system_info.os_name.as_str() {
            "Windows" => ObjectType::Pe,
            "iOS" | "macOS" => ObjectType::Macho,
            "Linux" | "Solaris" | "Android" => ObjectType::Elf,
            _ => ObjectType::Unknown,
        }
    }
}

struct SymbolicatorSymbolProvider {
    files: BTreeMap<DebugId, SymbolFile>,
    missing_ids: RwLock<BTreeSet<DebugId>>,
}

impl SymbolicatorSymbolProvider {
    /// Load the CFI information from the cache.
    ///
    /// This reads the CFI caches from disk and returns them in a format suitable for the
    /// processor to stackwalk.
    pub fn new<'a, M: Iterator<Item = (&'a DebugId, &'a Path)>>(modules: M) -> Self {
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
        _module: &(dyn Module + Sync),
        _frame: &mut (dyn FrameSymbolizer + Send),
    ) -> Result<(), FillSymbolError> {
        // Always return an error here to signal that we have no useful symbol information to
        // contribute. Doing nothing and reporting Ok trips a check in rust_minidump's
        // instruction_seems_valid_by_symbols function that leads stack scanning to stop prematurely.
        // See https://github.com/rust-minidump/rust-minidump/blob/7eed71e4075e0a81696ccc307d6ac68920de5db5/minidump-processor/src/stackwalker/mod.rs#L295.
        //
        // TODO: implement this properly, i.e., use symbolic to actually fill in information.
        Err(FillSymbolError {})
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

/// A module which was referenced in a minidump and processing information for it.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct CfiModule {
    /// Combined features provided by all the DIFs we found for this module.
    features: ObjectFeatures,
    /// Status of the CFI or unwind information for this module.
    cfi_status: ObjectFileStatus,
    /// Path to the CFI file in our cache, if there was a cache.
    cfi_path: Option<PathBuf>,
    /// The DIF object candidates for for this module.
    cfi_candidates: AllObjectCandidates,
    /// Indicates if the module was scanned during stack unwinding.
    scanned: bool,
    /// Indicates if the CFI for this module was consulted during stack unwinding.
    cfi_used: bool,
}

/// The CFI modules referenced by a minidump for CFI processing.
///
/// This is continuously updated with [`CfiCacheResult`] from referenced modules that have not yet
/// been fetched.  It contains the CFI cache status of the modules and allows loading the CFI from
/// the caches for the correct minidump stackwalking.
///
/// It maintains the status of the object file availability itself as well as any features
/// provided by it.  This can later be used to compile the required modules information
/// needed for the final response on the JSON API.  See the [`ModuleListBuilder`] struct for
/// this.
#[derive(Clone, Debug, Default)]
struct CfiCacheModules {
    /// We have to make sure to hold onto a reference to the CfiCacheFile,
    /// to make sure it will not be evicted in the middle of reading it in the procspawn
    cache_files: Vec<Arc<CfiCacheFile>>,
    inner: BTreeMap<DebugId, CfiModule>,
}

impl CfiCacheModules {
    /// Creates a new CFI cache entries for code modules.
    fn new() -> Self {
        Self {
            cache_files: vec![],
            inner: Default::default(),
        }
    }

    /// Extend the CacheModules with the fetched caches represented by
    /// [`CfiCacheResult`].
    fn extend(&mut self, cfi_caches: Vec<CfiCacheResult>) {
        self.cache_files.extend(
            cfi_caches
                .iter()
                .filter_map(|(_, cache_result)| cache_result.as_ref().ok())
                .map(Arc::clone),
        );

        let iter = cfi_caches.into_iter().map(|(code_id, cache_result)| {
            let cfi_module = match cache_result {
                Ok(cfi_cache) => {
                    let cfi_status = match cfi_cache.status() {
                        CacheStatus::Positive => ObjectFileStatus::Found,
                        CacheStatus::Negative => ObjectFileStatus::Missing,
                        CacheStatus::Malformed(details) => {
                            let err = CfiCacheError::ObjectParsing(ObjectError::Malformed);
                            let stderr: &dyn std::error::Error = &err;
                            tracing::warn!(stderr, "Error while parsing cficache: {}", details);
                            ObjectFileStatus::from(&err)
                        }
                        // If the cache entry is for a cache specific error, it must be
                        // from a previous cficache conversion attempt.
                        CacheStatus::CacheSpecificError(details) => {
                            let err = CfiCacheError::ObjectParsing(ObjectError::Malformed);
                            tracing::warn!("Cached error from parsing cficache: {}", details);
                            ObjectFileStatus::from(&err)
                        }
                    };
                    let cfi_path = match cfi_cache.status() {
                        CacheStatus::Positive => Some(cfi_cache.path().to_owned()),
                        _ => None,
                    };
                    CfiModule {
                        features: cfi_cache.features(),
                        cfi_status,
                        cfi_path,
                        cfi_candidates: cfi_cache.candidates().clone(), // TODO(flub): fix clone
                        ..Default::default()
                    }
                }
                Err(err) => {
                    let stderr: &dyn std::error::Error = &err;
                    tracing::debug!(stderr, "Error while fetching cficache");
                    CfiModule {
                        cfi_status: ObjectFileStatus::from(err.as_ref()),
                        ..Default::default()
                    }
                }
            };
            (code_id, cfi_module)
        });
        self.inner.extend(iter)
    }

    /// Returns a mapping of module IDs to paths that can then be loaded inside a procspawn closure.
    fn for_processing(&self) -> impl Iterator<Item = (&DebugId, &Path)> {
        self.inner
            .iter()
            .filter_map(|(id, module)| Some((id, module.cfi_path.as_deref()?)))
    }

    /// Returns the inner Map.
    fn into_inner(self) -> BTreeMap<DebugId, CfiModule> {
        self.inner
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

async fn stackwalk(
    cfi_caches: CfiCacheModules,
    minidump_path: PathBuf,
    return_modules: bool,
) -> anyhow::Result<StackWalkMinidumpResult> {
    // Stackwalk the minidump.
    let duration = Instant::now();
    let minidump = Minidump::read(ByteView::open(minidump_path)?)?;
    let provider = SymbolicatorSymbolProvider::new(cfi_caches.for_processing());
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
        cfi_caches,
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
    #[tracing::instrument(skip_all)]
    async fn load_cfi_caches(
        &self,
        scope: Scope,
        requests: &[(DebugId, &RawObjectInfo)],
        sources: Arc<[SourceConfig]>,
    ) -> Vec<CfiCacheResult> {
        let futures = requests.iter().map(|(id, object_info)| {
            let sources = sources.clone();
            let scope = scope.clone();

            async move {
                let result = self
                    .cficaches
                    .fetch(FetchCfiCache {
                        object_type: object_info.ty,
                        identifier: object_id_from_object_info(object_info),
                        sources,
                        scope,
                    })
                    .await;
                ((*id).to_owned(), result)
            }
            .bind_hub(Hub::new_from_top(Hub::current()))
        });

        future::join_all(futures).await
    }

    /// Saves the given `minidump_file` in the diagnostics cache if configured to do so.
    fn maybe_persist_minidump(&self, minidump_file: TempPath) {
        if let Some(dir) = self.diagnostics_cache.cache_dir() {
            if let Some(file_name) = minidump_file.file_name() {
                let path = dir.join(file_name);
                match minidump_file.persist(&path) {
                    Ok(_) => {
                        sentry::configure_scope(|scope| {
                            scope.set_extra(
                                "crashed_minidump",
                                sentry::protocol::Value::String(path.to_string_lossy().to_string()),
                            );
                        });
                    }
                    Err(e) => tracing::error!("Failed to save minidump {:?}", &e),
                };
            }
        } else {
            tracing::debug!("No diagnostics retention configured, not saving minidump");
        }
    }

    async fn do_process_minidump(
        &self,
        scope: Scope,
        minidump_file: TempPath,
        sources: Arc<[SourceConfig]>,
        options: RequestOptions,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let (request, state) = self
            .do_stackwalk_minidump(scope, minidump_file, sources, options)
            .await?;

        let mut response = self.do_symbolicate(request).await?;
        state.merge_into(&mut response);

        Ok(response)
    }

    /// Creates a new request to process a minidump.
    ///
    /// Returns `None` if the `SymbolicationActor` is already processing the
    /// maximum number of requests, as given by `max_concurrent_requests`.
    pub fn process_minidump(
        &self,
        scope: Scope,
        minidump_file: TempPath,
        sources: Arc<[SourceConfig]>,
        options: RequestOptions,
    ) -> Result<RequestId, MaxRequestsError> {
        let slf = self.clone();
        let span = sentry::configure_scope(|scope| scope.get_span());
        let ctx = sentry::TransactionContext::continue_from_span(
            "process_minidump",
            "process_minidump",
            span,
        );
        self.create_symbolication_request(async move {
            let transaction = sentry::start_transaction(ctx);
            sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));
            let res = slf
                .do_process_minidump(scope, minidump_file, sources, options)
                .await;
            transaction.finish();
            res
        })
    }

    /// Iteratively stackwalks/processes the given `minidump_file`.
    async fn stackwalk_minidump_iteratively(
        &self,
        scope: Scope,
        minidump_path: &Path,
        sources: Arc<[SourceConfig]>,
    ) -> anyhow::Result<StackWalkMinidumpResult> {
        let mut iterations = 0;

        let mut cfi_caches = CfiCacheModules::new();
        let mut modules: Option<HashMap<DebugId, RawObjectInfo>> = None;

        let mut result = loop {
            iterations += 1;

            let mut result = {
                let return_modules = modules.is_none();
                let minidump_path = minidump_path.to_path_buf();

                let future = stackwalk(cfi_caches, minidump_path, return_modules)
                    .bind_hub(sentry::Hub::current());
                tokio::time::timeout(Duration::from_secs(60), self.cpu_pool.spawn(future))
                    .await???
            };

            metric!(timer("minidump.stackwalk.duration") = result.duration);

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

            // break if no new cfi caches were successfully loaded
            if loaded_caches.iter().all(|(_, result)| match result {
                Err(_) => true,
                Ok(cfi_cache_file) => cfi_cache_file.status() != &CacheStatus::Positive,
            }) {
                break result;
            }
            cfi_caches = std::mem::take(&mut result.cfi_caches);
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

            let future =
                self.stackwalk_minidump_iteratively(scope.clone(), &minidump_file, sources.clone());

            let result = match future.await {
                Ok(result) => result,
                Err(err) => {
                    self.maybe_persist_minidump(minidump_file);
                    return Err(err);
                }
            };

            let StackWalkMinidumpResult {
                cfi_caches,
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

/// Merges the stacktraces processed via rust-minidump with the ones captured on the client.
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

fn normalize_minidump_os_name(os: Os) -> &'static str {
    // Be aware that MinidumpState::object_type matches on names produced here.
    match os {
        Os::Windows => "Windows",
        Os::MacOs => "macOS",
        Os::Ios => "iOS",
        Os::Linux => "Linux",
        Os::Solaris => "Solaris",
        Os::Android => "Android",
        Os::Ps3 => "PS3",
        Os::NaCl => "NaCl",
        Os::Unknown(_) => "",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::sync::Arc;

    use tempfile::NamedTempFile;

    use super::*;
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
