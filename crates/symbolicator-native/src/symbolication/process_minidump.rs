use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use minidump::system_info::Os;
use minidump::{MinidumpContext, MinidumpSystemInfo};
use minidump::{MinidumpModule, Module};
use minidump_processor::ProcessState;
use minidump_unwind::{
    FileError, FileKind, FillSymbolError, FrameSymbolizer, FrameWalker, SymbolProvider,
};
use sentry::{Hub, SentryFutureExt};
use serde::{Deserialize, Serialize};
use symbolic::common::{Arch, ByteView};
use symbolicator_service::metric;
use symbolicator_service::types::{
    ObjectFileStatus, Platform, RawObjectInfo, Scope, ScrapingConfig,
};
use symbolicator_service::utils::hex::HexValue;
use symbolicator_sources::{ObjectId, ObjectType, SourceConfig};
use tempfile::TempPath;
use tokio::sync::Notify;

use crate::caches::cficaches::{CfiCacheActor, CfiModuleInfo, FetchCfiCache, FetchedCfiCache};
use crate::interface::{
    CompleteObjectInfo, CompletedSymbolicationResponse, RawFrame, RawStacktrace, Registers,
    SymbolicateStacktraces, SystemInfo,
};
use crate::metrics::StacktraceOrigin;

use super::minidump_stacktraces::parse_stacktraces_from_minidump;
use super::module_lookup::object_file_status_from_cache_entry;
use super::symbolicate::SymbolicationActor;

type Minidump = minidump::Minidump<'static, ByteView<'static>>;

#[derive(Debug, Serialize, Deserialize)]
struct StackWalkMinidumpResult {
    modules: Vec<CompleteObjectInfo>,
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
#[derive(Debug, Serialize, Deserialize, PartialEq)]
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
                let msg = format!("Unknown minidump arch: {arch}");
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
                .exception_info
                .as_ref()
                .map(|info| format!("{} / {:#x}", info.reason, info.address.0))
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
}

/// The Key that is used for looking up the [`Module`] in the per-stackwalk CFI / computation cache.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct LookupKey {
    base_addr: u64,
    size: u64,
}

impl LookupKey {
    /// Creates a new lookup key for the given [`Module`].
    fn new(module: &(dyn Module)) -> Self {
        Self {
            base_addr: module.base_address(),
            size: module.size(),
        }
    }
}

#[derive(Clone)]
enum LazyCfiCache {
    Fetched(Arc<FetchedCfiCache>),
    Fetching(Arc<Notify>),
}

/// A [`SymbolProvider`] that uses a [`CfiCacheActor`] to fetch
/// CFI for stackwalking.
///
/// An instance of this type is always used to stackwalk one particular minidump.
struct SymbolicatorSymbolProvider {
    /// The scope of the stackwalking request.
    scope: Scope,
    /// The sources from which to fetch CFI.
    sources: Arc<[SourceConfig]>,
    /// The object type of the minidump to stackwalk.
    object_type: ObjectType,
    /// The actor used for fetching CFI.
    cficache_actor: CfiCacheActor,
    /// An internal database of loaded CFI.
    cficaches: Mutex<HashMap<LookupKey, LazyCfiCache>>,
}

impl SymbolicatorSymbolProvider {
    pub fn new(
        scope: Scope,
        sources: Arc<[SourceConfig]>,
        cficache_actor: CfiCacheActor,
        object_type: ObjectType,
    ) -> Self {
        Self {
            scope,
            sources,
            cficache_actor,
            object_type,
            cficaches: Default::default(),
        }
    }

    /// Fetches CFI for the given module, parses it into a `SymbolFile`, and stores it internally.
    async fn load_cfi_module(&self, module: &(dyn Module + Sync)) -> Arc<FetchedCfiCache> {
        let key = LookupKey::new(module);

        loop {
            // FIXME: ideally, we would like to only lock once here, but the borrow checker does not
            // understand that `drop()`-ing the guard before the `notified().await` does not hold
            // the guard across an `await` point.
            // Currently, it still thinks it does, and makes the resulting future `!Send`.
            // We will therefore double-lock again in the `None` branch.
            let entry = self.cficaches.lock().unwrap().get(&key).cloned();
            match entry {
                Some(LazyCfiCache::Fetched(cficache)) => return cficache,
                Some(LazyCfiCache::Fetching(notify)) => {
                    // unlock, wait and then try again
                    notify.notified().await;
                }
                None => {
                    // insert a notifier for concurrent accesses, then go on to actually fetch
                    let mut cficaches = self.cficaches.lock().unwrap();
                    cficaches.insert(key.clone(), LazyCfiCache::Fetching(Arc::new(Notify::new())));
                    break;
                }
            }
        }

        let sources = self.sources.clone();
        let scope = self.scope.clone();

        let code_file = non_empty_file_name(&module.code_file());
        let debug_file = module.debug_file().as_deref().and_then(non_empty_file_name);

        let identifier = ObjectId {
            code_id: module.code_identifier(),
            code_file,
            debug_id: module.debug_identifier(),
            debug_file,
            debug_checksum: None,
            object_type: self.object_type,
        };

        let cficache = self
            .cficache_actor
            .fetch(FetchCfiCache {
                object_type: self.object_type,
                identifier,
                sources,
                scope,
            })
            // NOTE: this `bind_hub` is important!
            // `load_cfi_module` is being called concurrently from `rust-minidump` via
            // `join_all`. We do need proper isolation of any async task that might
            // manipulate any Sentry scope.
            .bind_hub(Hub::new_from_top(Hub::current()))
            .await;

        let cficache = Arc::new(cficache);
        let mut cficaches = self.cficaches.lock().unwrap();
        if let Some(LazyCfiCache::Fetching(notify)) =
            cficaches.insert(key, LazyCfiCache::Fetched(cficache.clone()))
        {
            notify.notify_waiters();
        }
        cficache
    }
}

#[async_trait]
impl SymbolProvider for SymbolicatorSymbolProvider {
    async fn fill_symbol(
        &self,
        module: &(dyn Module + Sync),
        _frame: &mut (dyn FrameSymbolizer + Send),
    ) -> Result<(), FillSymbolError> {
        // Always return an error here to signal that we have no useful symbol information to
        // contribute. Doing nothing and reporting Ok trips a check in rust_minidump's
        // instruction_seems_valid_by_symbols function that leads stack scanning to stop prematurely.
        // See https://github.com/rust-minidump/rust-minidump/blob/7eed71e4075e0a81696ccc307d6ac68920de5db5/minidump-processor/src/stackwalker/mod.rs#L295.
        //
        // TODO: implement this properly, i.e., use symbolic to actually fill in information.

        // This function is being called for every context frame,
        // regardless if any stack walking happens.
        // In contrast, `walk_frame` below will be skipped in case the minidump
        // does not contain any actionable stack memory that would allow stack walking.
        // Loading this module here means we will be able to backfill a possibly missing
        // debug_id/file.
        self.load_cfi_module(module).await;
        Err(FillSymbolError {})
    }

    async fn walk_frame(
        &self,
        module: &(dyn Module + Sync),
        walker: &mut (dyn FrameWalker + Send),
    ) -> Option<()> {
        let cfi_module = self.load_cfi_module(module).await;
        cfi_module.cache.clone().ok()??.0.walk_frame(module, walker)
    }

    async fn get_file_path(
        &self,
        _module: &(dyn Module + Sync),
        _kind: FileKind,
    ) -> Result<PathBuf, FileError> {
        Err(FileError::NotFound)
    }
}

fn object_info_from_minidump_module(ty: ObjectType, module: &MinidumpModule) -> CompleteObjectInfo {
    // Some modules are not objects but rather fonts or JIT areas or other mmapped files
    // which we don't care about.  These may not have complete information so map these to
    // our schema by converting to None when needed.
    let code_id = module
        .code_identifier()
        .filter(|code_id| !code_id.is_nil())
        .map(|code_id| code_id.to_string().to_lowercase());
    let code_file = non_empty_file_name(&module.code_file());
    let debug_file = module.debug_file().as_deref().and_then(non_empty_file_name);

    CompleteObjectInfo::from(RawObjectInfo {
        ty,
        code_id,
        code_file,
        debug_id: module.debug_identifier().map(|c| c.breakpad().to_string()),
        debug_file,
        debug_checksum: None,
        image_addr: HexValue(module.base_address()),
        image_size: match module.size() {
            0 => None,
            size => Some(size),
        },
    })
}

async fn stackwalk(
    cficaches: CfiCacheActor,
    minidump: &Minidump,
    scope: Scope,
    sources: Arc<[SourceConfig]>,
) -> Result<StackWalkMinidumpResult> {
    // Stackwalk the minidump.
    let duration = Instant::now();
    let system_info = minidump
        .get_stream::<MinidumpSystemInfo>()
        .map_err(|_| minidump_processor::ProcessError::MissingSystemInfo)?;
    let ty = match system_info.os {
        Os::Windows => ObjectType::Pe,
        Os::MacOs | Os::Ios => ObjectType::Macho,
        Os::Linux | Os::Solaris | Os::Android => ObjectType::Elf,
        _ => ObjectType::Unknown,
    };
    let provider = SymbolicatorSymbolProvider::new(scope, sources, cficaches, ty);
    let process_state = minidump_processor::process_minidump(minidump, &provider).await?;
    let duration = duration.elapsed();

    let minidump_state = MinidumpState::from_process_state(&process_state);

    // Finally iterate through the threads and build the stacktraces to
    // return, marking modules as used when they are referenced by a frame.
    let requesting_thread_index: Option<usize> = process_state.requesting_thread;
    let threads = process_state.threads;
    let mut stacktraces = Vec::with_capacity(threads.len());
    for (index, thread) in threads.into_iter().enumerate() {
        let registers = match thread.frames.first() {
            Some(frame) => map_symbolic_registers(&frame.context),
            None => Registers::new(),
        };

        // We trim stack traces to 256 frames from the top. A similar limit is also in place in
        // relay / store normalization, so any excess frames will be thrown away by Sentry anyway.
        let frames = thread
            .frames
            .into_iter()
            .take(256)
            .map(|frame| {
                let package = frame
                    .module
                    .and_then(|module| non_empty_file_name(&module.code_file()));
                RawFrame {
                    instruction_addr: HexValue(frame.resume_address),
                    package,
                    trust: frame.trust.into(),
                    ..RawFrame::default()
                }
            })
            .collect();

        stacktraces.push(RawStacktrace {
            is_requesting: requesting_thread_index.map(|r| r == index),
            thread_name: thread.thread_name,
            thread_id: Some(thread.thread_id.into()),
            registers,
            frames,
        });
    }

    // Start building the module list for the symbolication response.
    // After stackwalking, `provider.cficaches` contains entries for exactly
    // those modules that were referenced by some stack frame in the minidump.
    let mut modules = vec![];
    let mut cficaches = provider.cficaches.into_inner().unwrap();
    for module in process_state.modules.by_addr() {
        let key = LookupKey::new(module);

        // Discard modules that weren't used and don't have any valid id to go by.
        let debug_id = module.debug_identifier();
        let code_id = module.code_identifier();
        if !cficaches.contains_key(&key) && debug_id.is_none() && code_id.is_none() {
            continue;
        }

        let mut obj_info = object_info_from_minidump_module(ty, module);

        let unwind_status = match cficaches.remove(&key) {
            Some(LazyCfiCache::Fetched(cfi_module)) => {
                let cfi_module = Arc::into_inner(cfi_module).unwrap();
                obj_info.features.merge(cfi_module.features);
                // NOTE: minidump stackwalking is the first thing that happens to a request,
                // hence the current candidate list is empty.
                obj_info.candidates = cfi_module.candidates;

                // if the debug_id/file is empty, it might be possible to
                // backfill that using the reference in the executable file.
                if let Ok(Some(cfi_item)) = &cfi_module.cache {
                    if let Some(cfi_module_info) = &cfi_item.1 {
                        maybe_backfill_debugid(&mut obj_info.raw, cfi_module_info);
                    }
                }

                object_file_status_from_cache_entry(&cfi_module.cache)
            }
            _ => ObjectFileStatus::Unused,
        };
        obj_info.unwind_status = Some(unwind_status);

        metric!(
            counter("symbolication.unwind_status") += 1,
            "status" => obj_info.unwind_status.unwrap_or(ObjectFileStatus::Unused).name(),
        );

        modules.push(obj_info);
    }

    Ok(StackWalkMinidumpResult {
        modules,
        stacktraces,
        minidump_state,
        duration,
    })
}

fn maybe_backfill_debugid(info: &mut RawObjectInfo, cfi_module: &CfiModuleInfo) {
    if info.debug_id.is_none() {
        info.debug_id = Some(cfi_module.debug_id.to_string());
    }
    if info.debug_file.is_none() {
        info.debug_file = Some(cfi_module.debug_file.clone());
    }
}

impl SymbolicationActor {
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

    pub async fn process_minidump(
        &self,
        platform: Option<Platform>,
        scope: Scope,
        minidump_file: TempPath,
        sources: Arc<[SourceConfig]>,
        scraping: ScrapingConfig,
    ) -> Result<CompletedSymbolicationResponse> {
        let (request, state) = self
            .stackwalk_minidump(platform, scope, minidump_file, sources, scraping)
            .await?;

        let mut response = self.symbolicate(request).await?;
        state.merge_into(&mut response);

        Ok(response)
    }

    #[tracing::instrument(skip_all)]
    async fn stackwalk_minidump(
        &self,
        platform: Option<Platform>,
        scope: Scope,
        minidump_file: TempPath,
        sources: Arc<[SourceConfig]>,
        scraping: ScrapingConfig,
    ) -> Result<(SymbolicateStacktraces, MinidumpState)> {
        let len = minidump_file.metadata()?.len();
        tracing::debug!("Processing minidump ({} bytes)", len);
        metric!(time_raw("minidump.upload.size") = len);

        let minidump_path = minidump_file.to_path_buf();

        let minidump = match read_minidump(&minidump_path) {
            Ok(md) => md,
            Err(err) => {
                self.maybe_persist_minidump(minidump_file);
                return Err(err);
            }
        };

        let stackwalk_future = stackwalk(
            self.cficaches.clone(),
            &minidump,
            scope.clone(),
            sources.clone(),
        );

        let result = match stackwalk_future.await {
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
            duration,
        } = result;

        metric!(timer("minidump.stackwalk.duration") = duration);

        match parse_stacktraces_from_minidump(&minidump) {
            Ok(Some(client_stacktraces)) => {
                merge_clientside_with_processed_stacktraces(&mut stacktraces, client_stacktraces)
            }
            Err(e) => tracing::error!("invalid minidump extension: {}", e),
            _ => (),
        }

        let request = SymbolicateStacktraces {
            platform,
            modules,
            scope,
            sources,
            origin: StacktraceOrigin::Minidump,
            signal: None,
            stacktraces,
            apply_source_context: true,
            scraping,
        };

        Ok((request, minidump_state))
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

fn read_minidump(path: &Path) -> Result<Minidump> {
    let bv = ByteView::open(path)?;
    let md = Minidump::read(bv)?;
    Ok(md)
}

/// Returns an owned version of `file_name`, or `None` if it is empty.
fn non_empty_file_name(file_name: &str) -> Option<String> {
    if file_name.is_empty() {
        return None;
    }
    Some(file_name.to_owned())
}
