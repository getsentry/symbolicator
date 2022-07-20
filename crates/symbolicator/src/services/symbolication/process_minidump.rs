use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::channel::oneshot;
use futures::future::Shared;
use futures::FutureExt;
use minidump::system_info::Os;
use minidump::{MinidumpContext, MinidumpSystemInfo};
use minidump::{MinidumpModule, Module};
use minidump_processor::{
    FileError, FileKind, FillSymbolError, FrameSymbolizer, FrameWalker, ProcessState, SymbolFile,
    SymbolProvider, SymbolStats,
};
use parking_lot::{Mutex, RwLock};
use sentry::types::DebugId;
use sentry::{Hub, SentryFutureExt};
use serde::{Deserialize, Serialize};
use symbolic::cfi::CfiCache;
use symbolic::common::{Arch, ByteView};
use tempfile::TempPath;

use crate::cache::CacheStatus;
use crate::services::cficaches::{CfiCacheActor, CfiCacheError, FetchCfiCache};
use crate::services::download::ObjectId;
use crate::services::minidump::parse_stacktraces_from_minidump;
use crate::services::objects::ObjectError;
use crate::sources::SourceConfig;
use crate::types::{
    AllObjectCandidates, CompleteObjectInfo, CompletedSymbolicationResponse, ObjectFeatures,
    ObjectFileStatus, ObjectType, RawFrame, RawObjectInfo, RawStacktrace, Registers,
    RequestOptions, Scope, SystemInfo,
};
use crate::utils::futures::{m, measure};
use crate::utils::hex::HexValue;

use super::{StacktraceOrigin, SymbolicateStacktraces, SymbolicationActor, SymbolicationError};

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
}

/// Loads a [`SymbolFile`] from the given `Path`.
#[tracing::instrument(skip_all, fields(cfi_cache = %cfi_path.to_string_lossy()))]
fn load_symbol_file(cfi_path: &Path) -> Result<SymbolFile, anyhow::Error> {
    sentry::with_scope(
        |scope| {
            scope.set_extra(
                "cfi_cache",
                sentry::protocol::Value::String(cfi_path.to_string_lossy().to_string()),
            )
        },
        || {
            let bytes = ByteView::open(&cfi_path).map_err(|err| {
                let stderr: &dyn std::error::Error = &err;
                tracing::error!(stderr, "Error while reading cficache");
                err
            })?;

            let cfi_cache = CfiCache::from_bytes(bytes)
                // This mostly never happens since we already checked the files
                // after downloading and they would have been tagged with
                // CacheStatus::Malformed.
                .map_err(|err| {
                    let stderr: &dyn std::error::Error = &err;
                    tracing::error!(stderr, "Error while loading cficache");
                    err
                })?;

            if cfi_cache.as_slice().is_empty() {
                anyhow::bail!("cficache is empty")
            }

            let symbol_file = SymbolFile::from_bytes(cfi_cache.as_slice()).map_err(|err| {
                let stderr: &dyn std::error::Error = &err;
                tracing::error!(stderr, "Error while processing cficache");
                err
            })?;

            Ok(symbol_file)
        },
    )
}

/// Processing information for a module that was referenced in a minidump.
#[derive(Debug, Default)]
struct CfiModule {
    /// Combined features provided by all the DIFs we found for this module.
    features: ObjectFeatures,
    /// Status of the CFI or unwind information for this module.
    cfi_status: ObjectFileStatus,
    /// Call frame information for the module, loaded as a [`SymbolFile`].
    symbol_file: Option<SymbolFile>,
    /// The DIF object candidates for for this module.
    cfi_candidates: AllObjectCandidates,
}

type SymbolFileComputation = Shared<oneshot::Receiver<()>>;

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
    ///
    /// The key consists of a module's debug identifier and base address.
    cficaches: RwLock<HashMap<(DebugId, u64), CfiModule>>,
    running_computations: Mutex<HashMap<(DebugId, u64), SymbolFileComputation>>,
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
            running_computations: Default::default(),
        }
    }

    /// Fetches CFI for the given module, parses it into a `SymbolFile`, and stores it internally.
    async fn load_cfi_module(&self, module: &(dyn Module + Sync)) {
        let id = (
            module.debug_identifier().unwrap_or_default(),
            module.base_address(),
        );
        if self.cficaches.read().contains_key(&id) {
            return;
        }

        if id.0.is_nil() {
            // save a dummy "missing" CfiModule for this id.
            let cfi_module = CfiModule {
                cfi_status: ObjectFileStatus::Missing,
                ..Default::default()
            };
            self.cficaches.write().insert(id, cfi_module);
            return;
        }

        // Check if there is already a CFI cache/symbol file computation in progress for this module.
        // If not, start it and save it in `running_computations`.
        let maybe_computation = self.running_computations.lock().get(&id).cloned();
        match maybe_computation {
            Some(computation) => computation.await.expect("sender was dropped prematurely"),
            None => {
                let (sender, receiver) = oneshot::channel();
                self.running_computations
                    .lock()
                    .insert(id, receiver.shared());

                let sources = self.sources.clone();
                let scope = self.scope.clone();

                let identifier = ObjectId {
                    code_id: module.code_identifier(),
                    code_file: Some(module.code_file().into_owned()),
                    debug_id: module.debug_identifier(),
                    debug_file: module
                        .debug_file()
                        .map(|debug_file| debug_file.into_owned()),
                    object_type: self.object_type,
                };

                let cache_result = self
                    .cficache_actor
                    .fetch(FetchCfiCache {
                        object_type: self.object_type,
                        identifier,
                        sources,
                        scope,
                    })
                    .bind_hub(Hub::new_from_top(Hub::current()))
                    .await;

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
                        let symbol_file = match cfi_cache.status() {
                            CacheStatus::Positive => load_symbol_file(cfi_cache.path()).ok(),
                            _ => None,
                        };
                        CfiModule {
                            features: cfi_cache.features(),
                            cfi_status,
                            symbol_file,
                            cfi_candidates: cfi_cache.candidates().clone(), // TODO(flub): fix clone
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

                self.cficaches.write().insert(id, cfi_module);

                // Ignore error if receiver was dropped
                let _ = sender.send(());
            }
        }
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
        self.load_cfi_module(module).await;
        let id = (module.debug_identifier()?, module.base_address());
        self.cficaches
            .read()
            .get(&id)?
            .symbol_file
            .as_ref()?
            .walk_frame(module, walker)
    }

    fn stats(&self) -> HashMap<String, SymbolStats> {
        self.cficaches
            .read()
            .iter()
            .filter_map(|((debug_id, _), sym)| {
                let stats = SymbolStats {
                    symbol_url: sym.symbol_file.as_ref()?.url.clone(), // TODO(ja): We could put our candidate URI here
                    loaded_symbols: true, // TODO(ja): Should we return `false` for not found?
                    corrupt_symbols: false,
                };

                Some((debug_id.to_string(), stats))
            })
            .collect()
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
    let code_file = module.code_file();
    let code_file = match code_file.is_empty() {
        true => None,
        false => Some(code_file.into_owned()),
    };

    CompleteObjectInfo::from(RawObjectInfo {
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
    })
}

async fn stackwalk(
    cficaches: CfiCacheActor,
    minidump_path: PathBuf,
    scope: Scope,
    sources: Arc<[SourceConfig]>,
) -> anyhow::Result<StackWalkMinidumpResult> {
    // Stackwalk the minidump.
    let duration = Instant::now();
    let minidump = Minidump::read(ByteView::open(minidump_path)?)?;
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
    let process_state = minidump_processor::process_minidump(&minidump, &provider).await?;
    let duration = duration.elapsed();

    let minidump_state = MinidumpState::from_process_state(&process_state);

    // Finally iterate through the threads and build the stacktraces to
    // return, marking modules as used when they are referenced by a frame.
    let requesting_thread_index: Option<usize> = process_state.requesting_thread;
    let threads = process_state.threads;
    let mut stacktraces = Vec::with_capacity(threads.len());
    for (index, thread) in threads.into_iter().enumerate() {
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
            thread_name: thread.thread_name,
            thread_id: Some(thread.thread_id.into()),
            registers,
            frames,
        });
    }

    // Start building the module list for the symbolication response.
    // After stackwalking, `provider.cficaches` contains entries for exactly
    // those modules that were referenced by some stack frame in the minidump.
    let mut cficaches = provider.cficaches.into_inner();
    let modules: Vec<CompleteObjectInfo> = process_state
        .modules
        .by_addr()
        .filter_map(|module| {
            let id = (
                module.debug_identifier().unwrap_or_default(),
                module.base_address(),
            );

            // Discard modules that weren't used and don't have a debug id.
            if !cficaches.contains_key(&id) && module.debug_identifier().is_none() {
                return None;
            }

            let mut obj_info = object_info_from_minidump_module(ty, module);

            obj_info.unwind_status = match cficaches.remove(&id) {
                None => Some(ObjectFileStatus::Unused),
                Some(cfi_module) => {
                    obj_info.features.merge(cfi_module.features);
                    obj_info.candidates = cfi_module.cfi_candidates;
                    Some(cfi_module.cfi_status)
                }
            };

            metric!(
                counter("symbolication.unwind_status") += 1,
                "status" => obj_info.unwind_status.unwrap_or(ObjectFileStatus::Unused).name(),
            );

            Some(obj_info)
        })
        .collect();

    Ok(StackWalkMinidumpResult {
        modules,
        stacktraces,
        minidump_state,
        duration,
    })
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

    pub(super) async fn do_process_minidump(
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

    #[tracing::instrument(skip_all)]
    async fn do_stackwalk_minidump(
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

            let future = stackwalk(
                self.cficaches.clone(),
                minidump_file.to_path_buf(),
                scope.clone(),
                sources.clone(),
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
                duration,
            } = result;

            metric!(timer("minidump.stackwalk.duration") = duration);

            match parse_stacktraces_from_minidump(&ByteView::open(&minidump_file)?) {
                Ok(Some(client_stacktraces)) => merge_clientside_with_processed_stacktraces(
                    &mut stacktraces,
                    client_stacktraces,
                ),
                Err(e) => tracing::error!("invalid minidump extension: {}", e),
                _ => (),
            }

            let request = SymbolicateStacktraces {
                modules,
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

    use crate::services::symbolication::tests::setup_service;
    use crate::test;
    use crate::types::{RequestOptions, Scope};

    macro_rules! assert_snapshot {
        ($e:expr) => {
            ::insta::assert_yaml_snapshot!($e, {
                ".**.location" => ::insta::dynamic_redaction(
                    $crate::services::symbolication::tests::redact_localhost_port
                )
            });
        }
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
