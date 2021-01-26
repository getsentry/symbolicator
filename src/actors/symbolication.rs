use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;
use std::future::Future;
use std::io::Write;
use std::iter::FromIterator;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use apple_crash_report_parser::AppleCrashReport;
use bytes::{Bytes, IntoBuf};
use chrono::{DateTime, TimeZone, Utc};
use futures::{channel::oneshot, future, FutureExt as _};
use parking_lot::Mutex;
use regex::Regex;
use sentry::protocol::SessionStatus;
use sentry::{Hub, SentryFutureExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use symbolic::common::{
    Arch, ByteView, CodeId, DebugId, InstructionInfo, Language, Name, SelfCell,
};
use symbolic::debuginfo::{Object, ObjectDebugSession};
use symbolic::demangle::{Demangle, DemangleOptions};
use symbolic::minidump::cfi::CfiCache;
use symbolic::minidump::processor::{
    CodeModule, CodeModuleId, FrameTrust, ProcessMinidumpError, ProcessState, RegVal,
};
use thiserror::Error;

use crate::actors::cficaches::{CfiCacheActor, CfiCacheError, CfiCacheFile, FetchCfiCache};
use crate::actors::objects::{FindObject, ObjectError, ObjectPurpose, ObjectsActor};
use crate::actors::symcaches::{FetchSymCache, SymCacheActor, SymCacheError, SymCacheFile};
use crate::cache::CacheStatus;
use crate::logging::LogError;
use crate::sources::{FileType, SourceConfig};
use crate::types::{
    CompleteObjectInfo, CompleteStacktrace, CompletedSymbolicationResponse, FrameStatus,
    ObjectFileStatus, ObjectId, ObjectType, RawFrame, RawObjectInfo, RawStacktrace, Registers,
    RequestId, RequestOptions, Scope, Signal, SymbolicatedFrame, SymbolicationResponse, SystemInfo,
};
use crate::utils::addr::AddrMode;
use crate::utils::futures::{
    delay, m, measure, spawn_compat, timeout_compat, CallOnDrop, ThreadPool,
};
use crate::utils::hex::HexValue;

/// Options for demangling all symbols.
const DEMANGLE_OPTIONS: DemangleOptions = DemangleOptions::complete().return_type(false);

/// The maximum delay we allow for polling a finished request before dropping it.
const MAX_POLL_DELAY: Duration = Duration::from_secs(90);

lazy_static::lazy_static! {
    /// Format sent by Unreal Engine on macOS
    static ref OS_MACOS_REGEX: Regex = Regex::new(r#"^Mac OS X (?P<version>\d+\.\d+\.\d+)( \((?P<build>[a-fA-F0-9]+)\))?$"#).unwrap();
}

/// Errors during symbolication.
#[derive(Debug, Error)]
pub enum SymbolicationError {
    #[error("symbolication took too long")]
    Timeout,

    #[error(transparent)]
    Failed(#[from] anyhow::Error),

    #[error("failed to process minidump")]
    InvalidMinidump(#[from] ProcessMinidumpError),

    #[error("failed to parse apple crash report")]
    InvalidAppleCrashReport(#[from] apple_crash_report_parser::ParseError),
}

impl SymbolicationError {
    fn to_symbolication_response(&self) -> SymbolicationResponse {
        match self {
            SymbolicationError::Timeout => SymbolicationResponse::Timeout,
            SymbolicationError::Failed(_) => SymbolicationResponse::InternalError,
            SymbolicationError::InvalidMinidump(_) => SymbolicationResponse::Failed {
                message: self.to_string(),
            },
            SymbolicationError::InvalidAppleCrashReport(_) => SymbolicationResponse::Failed {
                message: self.to_string(),
            },
        }
    }
}

// We want a shared future here because otherwise polling for a response would hold the global lock.
type ComputationChannel = future::Shared<oneshot::Receiver<(Instant, SymbolicationResponse)>>;

type ComputationMap = Arc<Mutex<BTreeMap<RequestId, ComputationChannel>>>;

#[derive(Debug, Clone)]
pub struct SymCacheLookupResult<'a> {
    module_index: usize,
    object_info: &'a CompleteObjectInfo,
    symcache: Option<&'a SymCacheFile>,
    relative_addr: Option<u64>,
}

impl<'a> SymCacheLookupResult<'a> {
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

/// A builder for the modules list of the symbolication response.
///
/// On several platforms, the list of code modules can include shared memory regions or memory
/// mapped fonts. These are not valid code modules and should be excluded from the list. In some
/// cases, however, actual modules end up being mapped from shared memory. In such a case, stack
/// frames reference code modules without a provided code identifier.
///
/// This type exposes [`CodeModulesBuilder::build`], which returns all modules that either
/// have a valid identifier, or that are referenced from a stack trace. Use
/// [`CodeModulesBuilder::mark_referenced`] to indicate that a frame references a module.
struct CodeModulesBuilder {
    inner: Vec<(CompleteObjectInfo, bool)>,
}

impl CodeModulesBuilder {
    /// Creates a new [`CodeModulesBuilder`] from a list of code modules.
    pub fn new<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = CompleteObjectInfo>,
    {
        let mut inner = iter
            .into_iter()
            .map(|info| (info, false))
            .collect::<Vec<_>>();

        // Sort by image address for binary search in `mark`.
        inner.sort_by_key(|(info, _)| info.raw.image_addr);
        Self { inner }
    }

    /// Records a module covering the given address as referenced.
    ///
    /// The respective module will always be included in the final list of modules.
    pub fn mark_referenced(&mut self, addr: u64) {
        let search_index = self
            .inner
            .binary_search_by_key(&addr, |(info, _)| info.raw.image_addr.0);

        let info_index = match search_index {
            Ok(index) => index,
            Err(0) => return,
            Err(index) => index - 1,
        };

        let (info, marked) = &mut self.inner[info_index];
        let HexValue(image_addr) = info.raw.image_addr;
        let should_mark = match info.raw.image_size {
            Some(size) => addr < image_addr + size,
            // If there is no image size, the image implicitly counts up to the next image. Because
            // we know that the search address is somewhere in this range, we can mark it.
            None => true,
        };

        if should_mark {
            *marked = true;
        }
    }

    /// Returns the modules list to be used in the symbolication response.
    pub fn build(self) -> Vec<CompleteObjectInfo> {
        self.inner
            .into_iter()
            .filter(|(info, marked)| *marked || info.raw.debug_id.is_some())
            .map(|(info, _)| info)
            .collect()
    }
}

impl FromIterator<CompleteObjectInfo> for CodeModulesBuilder {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = CompleteObjectInfo>,
    {
        Self::new(iter)
    }
}

#[derive(Clone, Debug)]
pub struct SymbolicationActor {
    objects: ObjectsActor,
    symcaches: SymCacheActor,
    cficaches: CfiCacheActor,
    diagnostics_cache: crate::cache::Cache,
    threadpool: ThreadPool,
    requests: ComputationMap,
    spawnpool: Arc<procspawn::Pool>,
}

impl SymbolicationActor {
    pub fn new(
        objects: ObjectsActor,
        symcaches: SymCacheActor,
        cficaches: CfiCacheActor,
        diagnostics_cache: crate::cache::Cache,
        threadpool: ThreadPool,
        spawnpool: procspawn::Pool,
    ) -> Self {
        SymbolicationActor {
            objects,
            symcaches,
            cficaches,
            diagnostics_cache,
            threadpool,
            requests: Arc::new(Mutex::new(BTreeMap::new())),
            spawnpool: Arc::new(spawnpool),
        }
    }

    fn create_symbolication_request<F>(&self, f: F) -> RequestId
    where
        F: Future<Output = Result<CompletedSymbolicationResponse, SymbolicationError>> + 'static,
    {
        let (sender, receiver) = oneshot::channel();

        let hub = Arc::new(sentry::Hub::new_from_top(sentry::Hub::current()));

        // Assume that there are no UUID4 collisions in practice.
        let requests = self.requests.clone();
        let request_id = RequestId::new(uuid::Uuid::new_v4());
        requests.lock().insert(request_id, receiver.shared());
        let drop_hub = hub.clone();
        let token = CallOnDrop::new(move || {
            requests.lock().remove(&request_id);
            // we consider every premature drop of the future as fatal crash, which works fine
            // since ending a session consumes it and its not possible to double-end.
            drop_hub.end_session_with_status(SessionStatus::Crashed);
        });

        let request_future = async move {
            let response = match f.await {
                Ok(response) => {
                    sentry::end_session_with_status(SessionStatus::Exited);
                    SymbolicationResponse::Completed(Box::new(response))
                }
                Err(error) => {
                    // a timeout is an abnormal session exit, all other errors are considered "crashed"
                    let status = match &error {
                        SymbolicationError::Timeout => SessionStatus::Abnormal,
                        _ => SessionStatus::Crashed,
                    };
                    sentry::end_session_with_status(status);

                    let response = error.to_symbolication_response();
                    log::error!("Symbolication error: {:?}", anyhow::Error::new(error));
                    response
                }
            };

            sender.send((Instant::now(), response)).ok();

            // Wait before removing the channel from the computation map to allow clients to
            // poll the status.
            delay(MAX_POLL_DELAY).await;

            drop(token);
        }
        .bind_hub(hub);

        // TODO: This spawns into the current_thread runtime of the caller, which usually is the web
        // handler. This doesn't block the web request, but it congests the threads that should only
        // do web I/O. Instead, this should spawn into a dedicated resource (e.g. a threadpool) to
        // keep web requests flowing while symbolication tasks may backlog.
        spawn_compat(request_future);

        request_id
    }
}

async fn wrap_response_channel(
    request_id: RequestId,
    timeout: Option<u64>,
    channel: ComputationChannel,
) -> SymbolicationResponse {
    let channel_result = if let Some(timeout) = timeout {
        match timeout_compat(Duration::from_secs(timeout), channel).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                return SymbolicationResponse::Pending {
                    request_id,
                    // We should estimate this better, but at some point the
                    // architecture will probably change to pushing results on a
                    // queue instead of polling so it's unlikely we'll ever do
                    // better here.
                    retry_after: 30,
                };
            }
        }
    } else {
        channel.await
    };

    match channel_result {
        Ok((finished_at, response)) => {
            metric!(timer("requests.response_idling") = finished_at.elapsed());
            response
        }
        // If the sender is dropped, this is likely due to a panic that is captured at the source.
        // Therefore, we do not need to capture an error at this point.
        Err(_canceled) => SymbolicationResponse::InternalError,
    }
}

fn object_id_from_object_info(object_info: &RawObjectInfo) -> ObjectId {
    ObjectId {
        debug_id: match object_info.debug_id.as_deref() {
            None | Some("") => None,
            Some(string) => string.parse().ok(),
        },
        code_id: match object_info.code_id.as_deref() {
            None | Some("") => None,
            Some(string) => string.parse().ok(),
        },
        debug_file: object_info.debug_file.clone(),
        code_file: object_info.code_file.clone(),
        object_type: object_info.ty,
    }
}

fn normalize_minidump_os_name(minidump_os_name: &str) -> &str {
    // Be aware that MinidumpState::object_type matches on names produced here.
    match minidump_os_name {
        "Windows NT" => "Windows",
        "Mac OS X" => "macOS",
        _ => minidump_os_name,
    }
}

fn object_info_from_minidump_module(ty: ObjectType, module: &CodeModule) -> RawObjectInfo {
    let mut code_id = module.code_identifier();

    // The processor reports an empty string as code id for MachO files
    if ty == ObjectType::Macho && code_id.is_empty() {
        code_id = module.debug_identifier();
        code_id.truncate(code_id.len().max(1) - 1);
    }

    RawObjectInfo {
        ty,
        code_id: Some(code_id),
        code_file: Some(module.code_file()),
        debug_id: Some(module.debug_identifier()), // TODO: This should use module.id().map(_)
        debug_file: Some(module.debug_file()),
        image_addr: HexValue(module.base_address()),
        image_size: match module.size() {
            0 => None,
            size => Some(size),
        },
    }
}

pub struct SourceObject(SelfCell<ByteView<'static>, Object<'static>>);

struct SourceObjectEntry {
    module_index: usize,
    object_info: CompleteObjectInfo,
    source_object: Option<Arc<SourceObject>>,
}

struct SourceLookup {
    inner: Vec<SourceObjectEntry>,
}

impl SourceLookup {
    pub async fn fetch_sources(
        self,
        objects: ObjectsActor,
        scope: Scope,
        sources: Arc<[SourceConfig]>,
        response: &CompletedSymbolicationResponse,
    ) -> Result<Self, SymbolicationError> {
        let mut referenced_objects = BTreeSet::new();
        let stacktraces = &response.stacktraces;

        for stacktrace in stacktraces {
            for frame in &stacktrace.frames {
                if let Some(i) =
                    self.get_object_index_by_addr(frame.raw.instruction_addr.0, frame.raw.addr_mode)
                {
                    referenced_objects.insert(i);
                }
            }
        }

        let mut futures = Vec::new();

        for mut entry in self.inner.into_iter() {
            let is_used = referenced_objects.contains(&entry.module_index);
            let objects = objects.clone();
            let scope = scope.clone();
            let sources = sources.clone();

            futures.push(async move {
                if !is_used {
                    entry.object_info.debug_status = ObjectFileStatus::Unused;
                    entry.source_object = None;
                    return entry;
                }

                let opt_object_file_meta = objects
                    .clone()
                    .find(FindObject {
                        filetypes: FileType::sources(),
                        purpose: ObjectPurpose::Source,
                        scope: scope.clone(),
                        identifier: object_id_from_object_info(&entry.object_info.raw),
                        sources,
                    })
                    .await
                    .unwrap_or_default()
                    .meta;

                entry.source_object = match opt_object_file_meta {
                    None => None,
                    Some(object_file_meta) => {
                        objects.fetch(object_file_meta).await.ok().and_then(|x| {
                            SelfCell::try_new(x.data(), |b| Object::parse(unsafe { &*b }))
                                .map(|x| Arc::new(SourceObject(x)))
                                .ok()
                        })
                    }
                };

                if entry.source_object.is_some() {
                    entry.object_info.features.has_sources = true;
                }

                entry
            });
        }

        Ok(SourceLookup {
            inner: future::join_all(futures).await,
        })
    }

    pub fn prepare_debug_sessions(&self) -> Vec<Option<ObjectDebugSession<'_>>> {
        self.inner
            .iter()
            .map(|entry| {
                entry
                    .source_object
                    .as_ref()
                    .and_then(|o| o.0.get().debug_session().ok())
            })
            .collect()
    }

    pub fn get_context_lines(
        &self,
        debug_sessions: &[Option<ObjectDebugSession<'_>>],
        addr: u64,
        addr_mode: AddrMode,
        abs_path: &str,
        lineno: u32,
        n: usize,
    ) -> Option<(Vec<String>, String, Vec<String>)> {
        let index = self.get_object_index_by_addr(addr, addr_mode)?;
        let session = debug_sessions[index].as_ref()?;
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

    fn get_object_index_by_addr(&self, addr: u64, addr_mode: AddrMode) -> Option<usize> {
        match addr_mode {
            AddrMode::Abs => {
                for entry in self.inner.iter() {
                    let start_addr = entry.object_info.raw.image_addr.0;

                    if start_addr > addr {
                        // The debug image starts at a too high address
                        continue;
                    }

                    let size = entry.object_info.raw.image_size.unwrap_or(0);
                    if let Some(end_addr) = start_addr.checked_add(size) {
                        if end_addr < addr && size != 0 {
                            // The debug image ends at a too low address and we're also confident that
                            // end_addr is accurate (size != 0)
                            continue;
                        }
                    }

                    return Some(entry.module_index);
                }
                None
            }
            AddrMode::Rel(this_module_index) => self
                .inner
                .iter()
                .find(|x| x.module_index == this_module_index)
                .map(|x| x.module_index),
        }
    }

    fn sort(&mut self) {
        self.inner
            .sort_by_key(|entry| entry.object_info.raw.image_addr.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        self.inner.dedup_by(|entry2, entry1| {
            // If this underflows we didn't sort properly.
            let size = entry2.object_info.raw.image_addr.0 - entry1.object_info.raw.image_addr.0;
            if entry1.object_info.raw.image_size.unwrap_or(0) == 0 {
                entry1.object_info.raw.image_size = Some(size);
            }

            false
        });
    }
}

impl FromIterator<CompleteObjectInfo> for SourceLookup {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = CompleteObjectInfo>,
    {
        let mut rv = SourceLookup {
            inner: iter
                .into_iter()
                .enumerate()
                .map(|(module_index, object_info)| SourceObjectEntry {
                    module_index,
                    object_info,
                    source_object: None,
                })
                .collect(),
        };
        rv.sort();
        rv
    }
}

struct SymCacheEntry {
    module_index: usize,
    object_info: CompleteObjectInfo,
    symcache: Option<Arc<SymCacheFile>>,
}

struct SymCacheLookup {
    inner: Vec<SymCacheEntry>,
}

impl FromIterator<CompleteObjectInfo> for SymCacheLookup {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = CompleteObjectInfo>,
    {
        let mut rv = SymCacheLookup {
            inner: iter
                .into_iter()
                .enumerate()
                .map(|(module_index, object_info)| SymCacheEntry {
                    module_index,
                    object_info,
                    symcache: None,
                })
                .collect(),
        };
        rv.sort();
        rv
    }
}

impl SymCacheLookup {
    fn sort(&mut self) {
        self.inner
            .sort_by_key(|entry| entry.object_info.raw.image_addr.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        self.inner.dedup_by(|entry2, entry1| {
            // If this underflows we didn't sort properly.
            let size = entry2.object_info.raw.image_addr.0 - entry1.object_info.raw.image_addr.0;
            if entry1.object_info.raw.image_size.unwrap_or(0) == 0 {
                entry1.object_info.raw.image_size = Some(size);
            }

            false
        });
    }

    async fn fetch_symcaches(
        self,
        symcache_actor: SymCacheActor,
        request: SymbolicateStacktraces,
    ) -> Self {
        let mut referenced_objects = BTreeSet::new();
        let stacktraces = request.stacktraces;

        for stacktrace in stacktraces {
            for frame in stacktrace.frames {
                if let Some(SymCacheLookupResult { module_index, .. }) =
                    self.lookup_symcache(frame.instruction_addr.0, frame.addr_mode)
                {
                    referenced_objects.insert(module_index);
                }
            }
        }

        let mut futures = Vec::new();

        for mut entry in self.inner.into_iter() {
            let is_used = referenced_objects.contains(&entry.module_index);
            let sources = request.sources.clone();
            let scope = request.scope.clone();
            let symcache_actor = symcache_actor.clone();

            futures.push(async move {
                if !is_used {
                    entry.object_info.debug_status = ObjectFileStatus::Unused;
                    return entry;
                }
                let symcache_result = symcache_actor
                    .fetch(FetchSymCache {
                        object_type: entry.object_info.raw.ty,
                        identifier: object_id_from_object_info(&entry.object_info.raw),
                        sources,
                        scope,
                    })
                    .await;

                let (symcache, status) = match symcache_result {
                    Ok(symcache) => match symcache.parse() {
                        Ok(Some(_)) => (Some(symcache), ObjectFileStatus::Found),
                        Ok(None) => (Some(symcache), ObjectFileStatus::Missing),
                        Err(e) => (None, (&e).into()),
                    },
                    Err(e) => (None, (&*e).into()),
                };

                entry.object_info.arch = Default::default();

                if let Some(ref symcache) = symcache {
                    entry.object_info.arch = symcache.arch();
                    entry.object_info.features.merge(symcache.features());
                    entry.object_info.candidates.merge(symcache.candidates());
                }

                entry.symcache = symcache;
                entry.object_info.debug_status = status;
                entry
            });
        }

        SymCacheLookup {
            inner: future::join_all(futures).await,
        }
    }

    fn lookup_symcache(&self, addr: u64, addr_mode: AddrMode) -> Option<SymCacheLookupResult<'_>> {
        match addr_mode {
            AddrMode::Abs => {
                for entry in self.inner.iter() {
                    let start_addr = entry.object_info.raw.image_addr.0;

                    if start_addr > addr {
                        // The debug image starts at a too high address
                        continue;
                    }

                    let size = entry.object_info.raw.image_size.unwrap_or(0);
                    if let Some(end_addr) = start_addr.checked_add(size) {
                        if end_addr < addr && size != 0 {
                            // The debug image ends at a too low address and we're also confident that
                            // end_addr is accurate (size != 0)
                            continue;
                        }
                    }

                    return Some(SymCacheLookupResult {
                        module_index: entry.module_index,
                        object_info: &entry.object_info,
                        symcache: entry.symcache.as_deref(),
                        relative_addr: entry.object_info.abs_to_rel_addr(addr),
                    });
                }
                None
            }
            AddrMode::Rel(this_module_index) => self
                .inner
                .iter()
                .find(|x| x.module_index == this_module_index)
                .map(|entry| SymCacheLookupResult {
                    module_index: entry.module_index,
                    object_info: &entry.object_info,
                    symcache: entry.symcache.as_deref(),
                    relative_addr: Some(addr),
                }),
        }
    }
}

fn symbolicate_frame(
    caches: &SymCacheLookup,
    registers: &Registers,
    signal: Option<Signal>,
    frame: &mut RawFrame,
    index: usize,
) -> Result<Vec<SymbolicatedFrame>, FrameStatus> {
    let lookup_result = caches
        .lookup_symcache(frame.instruction_addr.0, frame.addr_mode)
        .ok_or(FrameStatus::UnknownImage)?;

    frame.package = lookup_result.object_info.raw.code_file.clone();
    if lookup_result.symcache.is_none() {
        if lookup_result.object_info.debug_status == ObjectFileStatus::Malformed {
            return Err(FrameStatus::Malformed);
        } else {
            return Err(FrameStatus::Missing);
        }
    }

    log::trace!("Loading symcache");
    let symcache = match lookup_result
        .symcache
        .as_ref()
        .expect("symcache should always be available at this point")
        .parse()
    {
        Ok(Some(x)) => x,
        Ok(None) => return Err(FrameStatus::Missing),
        Err(_) => return Err(FrameStatus::Malformed),
    };

    // get the relative caller address
    let relative_addr = if let Some(addr) = lookup_result.relative_addr {
        // heuristics currently are only supported when we can work with absolute addresses.
        // In cases where this is not possible we skip this part entirely and use the relative
        // address calculated by the lookup result as lookup address in the module.
        if let Some(absolute_addr) = lookup_result.object_info.rel_to_abs_addr(addr) {
            let is_crashing_frame = index == 0;
            let ip_register_value = if is_crashing_frame {
                symcache
                    .arch()
                    .cpu_family()
                    .ip_register_name()
                    .and_then(|ip_reg_name| registers.get(ip_reg_name))
                    .map(|x| x.0)
            } else {
                None
            };
            let absolute_caller_addr = InstructionInfo::new(symcache.arch(), absolute_addr)
                .is_crashing_frame(is_crashing_frame)
                .signal(signal.map(|signal| signal.0))
                .ip_register_value(ip_register_value)
                .caller_address();
            lookup_result
                .object_info
                .abs_to_rel_addr(absolute_caller_addr)
                .ok_or_else(|| {
                    log::warn!(
                            "Underflow when trying to subtract image start addr from caller address after heuristics"
                        );
                    metric!(counter("relative_addr.underflow") += 1);
                    FrameStatus::MissingSymbol
                })?
        } else {
            addr
        }
    } else {
        log::warn!("Underflow when trying to subtract image start addr from caller address before heuristics");
        metric!(counter("relative_addr.underflow") += 1);
        return Err(FrameStatus::MissingSymbol);
    };

    log::trace!("Symbolicating {:#x}", relative_addr);
    let line_infos = match symcache.lookup(relative_addr) {
        Ok(x) => x,
        Err(_) => return Err(FrameStatus::Malformed),
    };

    let mut rv = vec![];

    for line_info in line_infos {
        let line_info = match line_info {
            Ok(x) => x,
            Err(_) => return Err(FrameStatus::Malformed),
        };

        // The logic for filename and abs_path intentionally diverges from how symbolic is used
        // inside of Sentry right now.
        let (filename, abs_path) = {
            let rel_path = line_info.path();
            let abs_path = line_info.abs_path();

            if abs_path == rel_path {
                // rel_path is absolute and therefore not usable for `filename`. Use the
                // basename as filename.
                (line_info.filename().to_owned(), abs_path.to_owned())
            } else {
                // rel_path is relative (probably to the compilation dir) and therefore useful
                // as filename for Sentry.
                (rel_path.to_owned(), abs_path.to_owned())
            }
        };

        let name = line_info.function_name();

        // Detect the language from the bare name, ignoring any pre-set language. There are a few
        // languages that we should always be able to demangle. Only complain about those that we
        // detect explicitly, but silently ignore the rest. For instance, there are C-identifiers
        // reported as C++, which are expected not to demangle.
        let detected_language = Name::from(name.as_str()).detect_language();
        let should_demangle = match (line_info.language(), detected_language) {
            (_, Language::Unknown) => false, // can't demangle what we cannot detect
            (Language::ObjCpp, Language::Cpp) => true, // C++ demangles even if it was in ObjC++
            (Language::Unknown, _) => true,  // if there was no language, then rely on detection
            (lang, detected) => lang == detected, // avoid false-positive detections
        };

        let demangled_opt = name.demangle(DEMANGLE_OPTIONS);
        if should_demangle && demangled_opt.is_none() {
            sentry::with_scope(
                |scope| scope.set_extra("identifier", name.to_string().into()),
                || {
                    let message = format!("Failed to demangle {} identifier", line_info.language());
                    sentry::capture_message(&message, sentry::Level::Error);
                },
            );
        }

        rv.push(SymbolicatedFrame {
            status: FrameStatus::Symbolicated,
            original_index: Some(index),
            raw: RawFrame {
                package: lookup_result.object_info.raw.code_file.clone(),
                addr_mode: lookup_result.preferred_addr_mode(),
                instruction_addr: HexValue(
                    lookup_result.expose_preferred_addr(line_info.instruction_address()),
                ),
                symbol: Some(line_info.symbol().to_string()),
                abs_path: if !abs_path.is_empty() {
                    Some(abs_path)
                } else {
                    frame.abs_path.clone()
                },
                function: Some(match demangled_opt {
                    Some(demangled) => demangled,
                    None => name.into_cow().into_owned(),
                }),
                filename: if !filename.is_empty() {
                    Some(filename)
                } else {
                    frame.filename.clone()
                },
                lineno: Some(line_info.line()),
                pre_context: vec![],
                context_line: None,
                post_context: vec![],
                sym_addr: Some(HexValue(
                    lookup_result.expose_preferred_addr(line_info.function_address()),
                )),
                lang: match line_info.language() {
                    Language::Unknown => None,
                    language => Some(language),
                },
                trust: frame.trust,
            },
        });
    }

    if rv.is_empty() {
        return Err(FrameStatus::MissingSymbol);
    }

    Ok(rv)
}

fn symbolicate_stacktrace(
    thread: RawStacktrace,
    caches: &SymCacheLookup,
    signal: Option<Signal>,
) -> CompleteStacktrace {
    let mut stacktrace = CompleteStacktrace {
        thread_id: thread.thread_id,
        is_requesting: thread.is_requesting,
        registers: thread.registers.clone(),
        frames: vec![],
    };

    for (index, mut frame) in thread.frames.into_iter().enumerate() {
        match symbolicate_frame(caches, &thread.registers, signal, &mut frame, index) {
            Ok(frames) => stacktrace.frames.extend(frames),
            Err(status) => {
                // Since symbolication failed, the function name was not demangled. In case there is
                // either one of `function` or `symbol`, treat that as mangled name and try to
                // demangle it. If that succeeds, write the demangled name back.
                let mangled = frame.function.as_deref().xor(frame.symbol.as_deref());
                let demangled = mangled.and_then(|m| Name::from(m).demangle(DEMANGLE_OPTIONS));
                if let Some(demangled) = demangled {
                    if let Some(old_mangled) = frame.function.replace(demangled) {
                        frame.symbol = Some(old_mangled);
                    }
                }

                // Temporary workaround: Skip false-positive frames from stack scanning after the
                // fact.
                //
                // Usually, the stack scanner would skip all scanned frames when it *knows* that
                // they cannot be symbolized. However, in our case we supply breakpad symbols
                // without function records. This breaks its original heuristic, since it would now
                // *always* skip scan frames. Our patch in breakpad omits this check.
                //
                // Here, we fix this after the fact.
                //
                // - MissingSymbol: If symbolication failed for a scanned frame where we *know* we
                //   have a debug info, but the lookup inside that file failed.
                // - UnknownImage: If symbolication failed because the stackscanner found an
                //   instruction_addr that is not in any image *we* consider valid. We discard
                //   images which do not have a debug id, while the stackscanner considers them
                //   perfectly fine.
                if frame.trust == FrameTrust::Scan
                    && (status == FrameStatus::MissingSymbol || status == FrameStatus::UnknownImage)
                {
                    continue;
                }

                stacktrace.frames.push(SymbolicatedFrame {
                    status,
                    original_index: Some(index),
                    raw: frame,
                });
            }
        }
    }

    stacktrace
}

#[derive(Debug, Clone)]
/// A request for symbolication of multiple stack traces.
pub struct SymbolicateStacktraces {
    /// The scope of this request which determines access to cached files.
    pub scope: Scope,

    /// The signal thrown on certain operating systems.
    ///
    ///  Signal handlers sometimes mess with the runtime stack. This is used to determine whether
    /// the top frame should be fixed or not.
    pub signal: Option<Signal>,

    /// A list of external sources to load debug files.
    pub sources: Arc<[SourceConfig]>,

    /// A list of threads containing stack traces.
    pub stacktraces: Vec<RawStacktrace>,

    /// A list of images that were loaded into the process.
    ///
    /// This list must cover the instruction addresses of the frames in
    /// [`stacktraces`](Self::stacktraces). If a frame is not covered by any image, the frame cannot
    /// be symbolicated as it is not clear which debug file to load.
    pub modules: Vec<CompleteObjectInfo>,

    /// Options that came with this request, see [`RequestOptions`].
    pub options: RequestOptions,
}

impl SymbolicationActor {
    async fn do_symbolicate(
        self,
        request: SymbolicateStacktraces,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let serialize_dif_candidates = request.options.dif_candidates;

        let f = self.do_symbolicate_impl(request);
        let f = timeout_compat(Duration::from_secs(3600), f);
        let f = measure("symbolicate", m::timed_result, f);

        let mut response = f
            .await
            .map(|res| res.map_err(SymbolicationError::from))
            .unwrap_or(Err(SymbolicationError::Timeout))?;

        if !serialize_dif_candidates {
            response.clear_dif_candidates();
        }

        Ok(response)
    }

    async fn do_symbolicate_impl(
        self,
        request: SymbolicateStacktraces,
    ) -> Result<CompletedSymbolicationResponse, anyhow::Error> {
        let symcache_lookup: SymCacheLookup = request.modules.iter().cloned().collect();
        let source_lookup: SourceLookup = request.modules.iter().cloned().collect();
        let stacktraces = request.stacktraces.clone();
        let sources = request.sources.clone();
        let scope = request.scope.clone();
        let signal = request.signal;

        let symcache_lookup = symcache_lookup
            .fetch_symcaches(self.symcaches, request)
            .await;

        let future = async move {
            let stacktraces: Vec<_> = stacktraces
                .into_iter()
                .map(|trace| symbolicate_stacktrace(trace, &symcache_lookup, signal))
                .collect();

            let mut modules: Vec<_> = symcache_lookup
                .inner
                .into_iter()
                .map(|entry| {
                    metric!(
                        counter("symbolication.debug_status") += 1,
                        "status" => entry.object_info.debug_status.name()
                    );

                    (entry.module_index, entry.object_info)
                })
                .collect();

            // bring modules back into the original order
            modules.sort_by_key(|&(index, _)| index);
            let modules: Vec<_> = modules.into_iter().map(|(_, module)| module).collect();

            metric!(time_raw("symbolication.num_modules") = modules.len() as u64);
            metric!(time_raw("symbolication.num_stacktraces") = stacktraces.len() as u64);
            metric!(
                time_raw("symbolication.num_frames") =
                    stacktraces.iter().map(|s| s.frames.len() as u64).sum()
            );

            CompletedSymbolicationResponse {
                signal,
                modules,
                stacktraces,
                ..Default::default()
            }
        };

        let mut response = self
            .threadpool
            .spawn_handle(future.bind_hub(sentry::Hub::current()))
            .await
            .context("Symbolication future cancelled")?;

        let source_lookup = source_lookup
            .fetch_sources(self.objects, scope, sources, &response)
            .await?;

        let future = async move {
            let debug_sessions = source_lookup.prepare_debug_sessions();

            for trace in &mut response.stacktraces {
                for frame in &mut trace.frames {
                    let (abs_path, lineno) = match (&frame.raw.abs_path, frame.raw.lineno) {
                        (&Some(ref abs_path), Some(lineno)) => (abs_path, lineno),
                        _ => continue,
                    };

                    let result = source_lookup.get_context_lines(
                        &debug_sessions,
                        frame.raw.instruction_addr.0,
                        frame.raw.addr_mode,
                        abs_path,
                        lineno,
                        5,
                    );

                    if let Some((pre_context, context_line, post_context)) = result {
                        frame.raw.pre_context = pre_context;
                        frame.raw.context_line = Some(context_line);
                        frame.raw.post_context = post_context;
                    }
                }
            }
            response
        };

        self.threadpool
            .spawn_handle(future.bind_hub(sentry::Hub::current()))
            .await
            .context("Source lookup future cancelled")
    }

    pub fn symbolicate_stacktraces(&self, request: SymbolicateStacktraces) -> RequestId {
        self.create_symbolication_request(self.clone().do_symbolicate(request))
    }

    /// Polls the status for a started symbolication task.
    ///
    /// If the timeout is set and no result is ready within the given time,
    /// [`SymbolicationResponse::Pending`] is returned.
    // TODO(flub): once the callers are updated to be `async fn` this can take `&self` again.
    pub async fn get_response(
        self,
        request_id: RequestId,
        timeout: Option<u64>,
    ) -> Option<SymbolicationResponse> {
        let channel_opt = self.requests.lock().get(&request_id).cloned();
        match channel_opt {
            Some(channel) => Some(wrap_response_channel(request_id, timeout, channel).await),
            None => {
                // This is okay to occur during deploys, but if it happens all the time we have a state
                // bug somewhere. Could be a misconfigured load balancer (supposed to be pinned to
                // scopes).
                metric!(counter("symbolication.request_id_unknown") += 1);
                None
            }
        }
    }
}

type CfiCacheResult = (CodeModuleId, Result<Arc<CfiCacheFile>, Arc<CfiCacheError>>);

/// Contains some meta-data about a minidump.
///
/// The minidump meta-data contained here is extracted in a [`procspawn`] subprocess, so needs
/// to be (de)serialisable to/from JSON. It is only a way to get this metadata out of the
/// subprocess and merged into the final symbolication result.
///
/// A few more convenience methods exist to help with building the symbolication results.
#[derive(Debug, Serialize, Deserialize)]
struct MinidumpState {
    timestamp: DateTime<Utc>,
    system_info: SystemInfo,
    crashed: bool,
    crash_reason: String,
    assertion: String,
}

impl MinidumpState {
    /// Creates a new [`MinidumpState`] from a breakpad symbolication result.
    fn new(process_state: &ProcessState<'_>) -> Self {
        let minidump_system_info = process_state.system_info();
        let os_name = minidump_system_info.os_name();
        let os_version = minidump_system_info.os_version();
        let os_build = minidump_system_info.os_build();
        let cpu_family = minidump_system_info.cpu_family();
        let cpu_arch = match cpu_family.parse() {
            Ok(arch) => arch,
            Err(_) => {
                if !cpu_family.is_empty() {
                    let msg = format!("Unknown minidump arch: {}", cpu_family);
                    sentry::capture_message(&msg, sentry::Level::Error);
                }

                Default::default()
            }
        };
        MinidumpState {
            timestamp: Utc.timestamp(process_state.timestamp().try_into().unwrap_or_default(), 0),
            system_info: SystemInfo {
                os_name: normalize_minidump_os_name(&os_name).to_owned(),
                os_version,
                os_build,
                cpu_arch,
                device_model: String::default(),
            },
            crashed: process_state.crashed(),
            crash_reason: process_state.crash_reason(),
            assertion: process_state.assertion(),
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

impl SymbolicationActor {
    /// Extract the modules from a minidump.
    ///
    /// The modules are needed before we know which DIFs are needed to stackwalk this
    /// minidump.  The minidumps are processed in a subprocess to avoid crashes from the
    /// native library bringing down symbolicator.
    async fn get_referenced_modules_from_minidump(
        &self,
        minidump: Bytes,
    ) -> Result<Vec<(CodeModuleId, RawObjectInfo)>, anyhow::Error> {
        let pool = self.spawnpool.clone();
        let diagnostics_cache = self.diagnostics_cache.clone();
        let lazy = async move {
            let spawn_time = std::time::SystemTime::now();
            let spawn_result = pool.spawn(
                (minidump.clone(), spawn_time),
                |(minidump, spawn_time)| -> Result<_, ProcessMinidumpError> {
                    if let Ok(duration) = spawn_time.elapsed() {
                        metric!(timer("minidump.modules.spawn.duration") = duration);
                    }
                    log::debug!("Processing minidump ({} bytes)", minidump.len());
                    metric!(time_raw("minidump.upload.size") = minidump.len() as u64);
                    let state =
                        ProcessState::from_minidump(&ByteView::from_slice(&minidump), None)?;

                    let object_type = MinidumpState::new(&state).object_type();

                    let cfi_modules = state
                        .referenced_modules()
                        .into_iter()
                        .filter_map(|code_module| {
                            Some((
                                code_module.id()?,
                                object_info_from_minidump_module(object_type, code_module),
                            ))
                        })
                        .collect();

                    Ok(procspawn::serde::Json(cfi_modules))
                },
            );

            Self::join_procspawn(
                spawn_result,
                Duration::from_secs(20),
                "minidump.modules.spawn.error",
                minidump,
                diagnostics_cache,
            )
        };

        self.threadpool
            .spawn_handle(lazy.bind_hub(sentry::Hub::current()))
            .await
            .context("Getting minidump referenced modules future cancelled")?
    }

    /// Join a procspawn handle with a timeout.
    ///
    /// This handles the procspawn result, makes sure to appropriately log any failures and
    /// save the minidump for debugging.  Returns a simple result converted to the
    /// [`SymbolicationError`].
    fn join_procspawn<T, E>(
        handle: procspawn::JoinHandle<Result<procspawn::serde::Json<T>, E>>,
        timeout: Duration,
        metric: &str,
        minidump: Bytes,
        minidump_cache: crate::cache::Cache,
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
                if !perr.is_timeout() {
                    Self::save_minidump(minidump, minidump_cache)
                        .map_err(|e| log::error!("Failed to save minidump {:?}", &e))
                        .map(|r| {
                            if let Some(path) = r {
                                sentry::configure_scope(|scope| {
                                    scope.set_extra(
                                        "crashed_minidump",
                                        sentry::protocol::Value::String(
                                            path.to_string_lossy().to_string(),
                                        ),
                                    );
                                });
                            }
                        })
                        .ok();
                }
                Err(anyhow::Error::new(perr))
            }
        }
    }

    /// Save a minidump to temporary location.
    fn save_minidump(
        minidump: Bytes,
        failed_cache: crate::cache::Cache,
    ) -> anyhow::Result<Option<PathBuf>> {
        if let Some(dir) = failed_cache.cache_dir() {
            std::fs::create_dir_all(dir)?;
            let tmp = tempfile::Builder::new()
                .prefix("minidump")
                .suffix(".dmp")
                .tempfile_in(dir)?;
            tmp.as_file().write_all(&*minidump)?;
            let (_file, path) = tmp.keep().map_err(|e| e.error)?;
            Ok(Some(path))
        } else {
            log::debug!("No diagnostics retention configured, not saving minidump");
            Ok(None)
        }
    }

    async fn load_cfi_caches(
        &self,
        scope: Scope,
        requests: Vec<(CodeModuleId, RawObjectInfo)>,
        sources: Arc<[SourceConfig]>,
    ) -> Vec<CfiCacheResult> {
        let mut futures = Vec::with_capacity(requests.len());

        for (code_id, object_info) in requests {
            let sources = sources.clone();
            let scope = scope.clone();

            let fut = async move {
                let result = self
                    .cficaches
                    .fetch(FetchCfiCache {
                        object_type: object_info.ty,
                        identifier: object_id_from_object_info(&object_info),
                        sources,
                        scope,
                    })
                    .await;
                (code_id, result)
            };

            // Clone hub because of join_all concurrency.
            futures.push(fut.bind_hub(Hub::new_from_top(Hub::current())));
        }

        future::join_all(futures).await
    }

    /// Unwind the stack from a minidump.
    ///
    /// This processes the minidump to stackwalk all the threads found in the minidump.
    ///
    /// The `cfi_results` must contain all modules found in the minidump (extracted using
    /// [`SymbolicationActor::get_referenced_modules_from_minidump`]) and the result of trying
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
    async fn stackwalk_minidump_with_cfi(
        &self,
        scope: Scope,
        minidump: Bytes,
        sources: Arc<[SourceConfig]>,
        options: RequestOptions,
        cfi_results: Vec<CfiCacheResult>,
    ) -> Result<(SymbolicateStacktraces, MinidumpState), anyhow::Error> {
        let mut unwind_statuses = BTreeMap::new();
        let mut object_features = BTreeMap::new();
        let mut frame_info_map = BTreeMap::new();
        let mut dif_candidates = BTreeMap::new();

        // Go through all the modules in the minidump and build a map of the modules with
        // missing or malformed CFI.  ObjectFileStatus::Found is only added when the file is
        // loaded as it could still be corrupted when reading from the cache.  Usable CFI
        // cache files are added to the frame_info_map.
        for (code_module_id, result) in &cfi_results {
            let cache_file = match result {
                Ok(cfi_cache_file) => {
                    dif_candidates.insert(*code_module_id, cfi_cache_file.candidates().clone());
                    cfi_cache_file
                }
                Err(e) => {
                    log::debug!("Error while fetching cficache: {}", LogError(e.as_ref()));
                    unwind_statuses.insert(*code_module_id, (&**e).into());
                    continue;
                }
            };

            // NB: Always collect features, regardless of whether we fail to parse them or not.
            // This gives users the feedback that information is there but potentially not
            // processable by symbolicator.
            object_features.insert(*code_module_id, cache_file.features());

            match cache_file.status() {
                CacheStatus::Negative => {
                    unwind_statuses.insert(*code_module_id, ObjectFileStatus::Missing);
                }
                CacheStatus::Malformed => {
                    let e = CfiCacheError::ObjectParsing(ObjectError::Malformed);
                    log::warn!("Error while parsing cficache: {}", LogError(&e));
                    unwind_statuses.insert(*code_module_id, (&e).into());
                }
                CacheStatus::Positive => {
                    frame_info_map.insert(*code_module_id, cache_file.path().to_owned());
                }
            }
        }

        let pool = self.spawnpool.clone();
        let diagnostics_cache = self.diagnostics_cache.clone();
        let lazy = async move {
            let spawn_time = std::time::SystemTime::now();
            let spawn_result =
                pool.spawn(
                    (
                        frame_info_map,
                        object_features,
                        unwind_statuses,
                        minidump.clone(),
                        spawn_time,
                        procspawn::serde::Json(dif_candidates),
                    ),
                    |(
                        frame_info_map,
                        object_features,
                        mut unwind_statuses,
                        minidump,
                        spawn_time,
                        dif_candidates,
                    )|
                        -> Result<_, ProcessMinidumpError> {
                        let procspawn::serde::Json(mut dif_candidates) = dif_candidates;

                        if let Ok(duration) = spawn_time.elapsed() {
                            metric!(timer("minidump.stackwalk.spawn.duration") = duration);
                        }

                        // Load CFI caches from disk, updating unwind_statuses while doing so.
                        let mut cfi = BTreeMap::new();
                        for (code_module_id, cfi_path) in frame_info_map {
                            let result = ByteView::open(cfi_path)
                                .map_err(CfiCacheError::Io)
                                .and_then(|bytes| Ok(CfiCache::from_bytes(bytes)?));

                            match result {
                                Ok(cache) => {
                                    unwind_statuses.insert(code_module_id, ObjectFileStatus::Found);
                                    cfi.insert(code_module_id, cache);
                                }
                                Err(e) => {
                                    // This mostly never happens since we already checked
                                    // the files after downloading and they would have been
                                    // with tagged CacheStatus::Malformed.
                                    log::warn!("Error while reading cficache: {}", LogError(&e));
                                    unwind_statuses.insert(code_module_id, (&e).into());
                                }
                            }
                        }

                        // Stackwalk the minidump.
                        let minidump = ByteView::from_slice(&minidump);
                        let process_state = ProcessState::from_minidump(&minidump, Some(&cfi))?;
                        let threads = process_state.threads();

                        // Compute a list of all modules that result a scanned frame. Stack scanning
                        // walk from frame to frame, so missing unwind information results in the
                        // NEXT frame being scanned.
                        let mut scanned_modules = BTreeSet::new();
                        for thread in threads {
                            for window in thread.frames().windows(2) {
                                if let [prev, next] = window {
                                    if let Some(code_id) = prev.module().and_then(|m| m.id()) {
                                        let trust = next.trust();
                                        if matches!(trust, FrameTrust::Scan | FrameTrust::CFIScan) {
                                            scanned_modules.insert(code_id);
                                        }
                                    }
                                }
                            }
                        }

                        let minidump_state = MinidumpState::new(&process_state);
                        let object_type = minidump_state.object_type();

                        // Start building the module list to be returned in the
                        // symbolication response.  For all modules in the minidump we
                        // create the CompleteObjectInfo and start populating it.
                        let mut module_builder = process_state
                            .modules()
                            .into_iter()
                            .map(|code_module| {
                                let mut info: CompleteObjectInfo =
                                    object_info_from_minidump_module(object_type, code_module)
                                        .into();

                                let module_id = code_module.id();

                                let unwind_status = match module_id {
                                    Some(id) => match unwind_statuses.get(&id) {
                                        // The code module was not in the original set of referenced
                                        // modules, but was now required for stack walking.
                                        None if scanned_modules.contains(&id) => {
                                            sentry::capture_message("Referenced module not found during initial stack scan", sentry::Level::Error);
                                            ObjectFileStatus::Missing
                                        }
                                        // This code module was not referenced and therefore unused.
                                        None => ObjectFileStatus::Unused,
                                        // The unwind object was missing, but was not needed since
                                        // frames were not scanned. Assume it was unused.
                                        Some(ObjectFileStatus::Missing)
                                            if !scanned_modules.contains(&id) =>
                                        {
                                            ObjectFileStatus::Unused
                                        }
                                        // All other statuses like errors or OK are reported.
                                        Some(status) => *status,
                                    },
                                    // TODO: Emit a custom status for code modules without debug_id
                                    None => ObjectFileStatus::Missing,
                                };

                                metric!(
                                    counter("symbolication.unwind_status") += 1,
                                    "status" => unwind_status.name()
                                );
                                info.unwind_status = Some(unwind_status);

                                let features = match module_id {
                                    Some(id) => {
                                        object_features.get(&id).copied().unwrap_or_default()
                                    }
                                    None => Default::default(),
                                };

                                info.features.merge(features);

                                if let Some(code_id) = module_id {
                                    // If we have the same code module mapped into the
                                    // memory in multiple regions we would only have the
                                    // candidates filled in for the first one.
                                    if let Some(candidates) = dif_candidates.remove(&code_id) {
                                        info.candidates = candidates;
                                    }
                                }

                                info
                            })
                            .collect::<CodeModulesBuilder>();

                        // Finally iterate through the threads and build the stacktraces to
                        // return, marking used modules when they are referenced by a frame.
                        let requesting_thread_index: Option<usize> =
                            process_state.requesting_thread().try_into().ok();
                        let mut stacktraces = Vec::with_capacity(threads.len());
                        for (index, thread) in threads.iter().enumerate() {
                            let registers = match thread.frames().get(0) {
                                Some(frame) => map_symbolic_registers(
                                    frame.registers(minidump_state.system_info.cpu_arch),
                                ),
                                None => Registers::new(),
                            };

                            // Trim infinite recursions explicitly because those do not
                            // correlate to minidump size. Every other kind of bloated
                            // input data we know is already trimmed/rejected by raw
                            // byte size alone.
                            let frame_count = thread.frames().len().min(20000);
                            let mut frames = Vec::with_capacity(frame_count);
                            for frame in thread.frames().iter().take(frame_count) {
                                let return_address =
                                    frame.return_address(minidump_state.system_info.cpu_arch);
                                module_builder.mark_referenced(return_address);

                                frames.push(RawFrame {
                                    instruction_addr: HexValue(return_address),
                                    package: frame.module().map(CodeModule::code_file),
                                    trust: frame.trust(),
                                    ..RawFrame::default()
                                });
                            }

                            stacktraces.push(RawStacktrace {
                                is_requesting: requesting_thread_index.map(|r| r == index),
                                thread_id: Some(thread.thread_id().into()),
                                registers,
                                frames,
                            });
                        }

                        Ok(procspawn::serde::Json((
                            module_builder.build(),
                            stacktraces,
                            minidump_state,
                        )))
                    },
                );

            let (modules, stacktraces, minidump_state) = Self::join_procspawn(
                spawn_result,
                Duration::from_secs(60),
                "minidump.stackwalk.spawn.error",
                minidump,
                diagnostics_cache,
            )?;

            let request = SymbolicateStacktraces {
                modules,
                scope,
                sources,
                signal: None,
                stacktraces,
                options,
            };

            Ok((request, minidump_state))
        };

        let result = self
            .threadpool
            .spawn_handle(lazy.bind_hub(sentry::Hub::current()))
            .await
            .context("Minidump stackwalk future cancelled")?;

        // keep the results until symbolication has finished to ensure we don't drop
        // temporary files prematurely.
        drop(cfi_results);
        result
    }

    async fn do_stackwalk_minidump(
        self,
        scope: Scope,
        minidump: Bytes,
        sources: Vec<SourceConfig>,
        options: RequestOptions,
    ) -> Result<(SymbolicateStacktraces, MinidumpState), SymbolicationError> {
        let future = async move {
            let sources: Arc<[SourceConfig]> = Arc::from(sources);

            let referenced_modules = self
                .get_referenced_modules_from_minidump(minidump.clone())
                .await?;

            let cfi_caches = self
                .load_cfi_caches(scope.clone(), referenced_modules, sources.clone())
                .await;

            self.stackwalk_minidump_with_cfi(scope, minidump, sources, options, cfi_caches)
                .await
        };

        let future = timeout_compat(Duration::from_secs(3600), future);
        let future = measure("minidump_stackwalk", m::timed_result, future);
        future
            .await
            .map(|ret| ret.map_err(SymbolicationError::from))
            .unwrap_or(Err(SymbolicationError::Timeout))
    }

    async fn do_process_minidump(
        self,
        scope: Scope,
        minidump: Bytes,
        sources: Vec<SourceConfig>,
        options: RequestOptions,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let (request, state) = self
            .clone()
            .do_stackwalk_minidump(scope, minidump, sources, options)
            .await?;

        let mut response = self.do_symbolicate(request).await?;
        state.merge_into(&mut response);

        Ok(response)
    }

    pub fn process_minidump(
        &self,
        scope: Scope,
        minidump: Bytes,
        sources: Vec<SourceConfig>,
        options: RequestOptions,
    ) -> RequestId {
        self.create_symbolication_request(
            self.clone()
                .do_process_minidump(scope, minidump, sources, options),
        )
    }
}

#[derive(Debug)]
struct AppleCrashReportState {
    timestamp: Option<DateTime<Utc>>,
    system_info: SystemInfo,
    crash_reason: Option<String>,
    crash_details: Option<String>,
}

impl AppleCrashReportState {
    fn merge_into(mut self, response: &mut CompletedSymbolicationResponse) {
        if self.system_info.cpu_arch == Arch::Unknown {
            self.system_info.cpu_arch = response
                .modules
                .iter()
                .map(|object| object.arch)
                .find(|arch| *arch != Arch::Unknown)
                .unwrap_or_default();
        }

        response.timestamp = self.timestamp;
        response.system_info = Some(self.system_info);
        response.crash_reason = self.crash_reason;
        response.crash_details = self.crash_details;
        response.crashed = Some(true);
    }
}

fn map_apple_binary_image(image: apple_crash_report_parser::BinaryImage) -> CompleteObjectInfo {
    let code_id = CodeId::from_binary(&image.uuid.as_bytes()[..]);
    let debug_id = DebugId::from_uuid(image.uuid);

    let raw_info = RawObjectInfo {
        ty: ObjectType::Macho,
        code_id: Some(code_id.to_string()),
        code_file: Some(image.path.clone()),
        debug_id: Some(debug_id.to_string()),
        debug_file: Some(image.path),
        image_addr: HexValue(image.addr.0),
        image_size: match image.size {
            0 => None,
            size => Some(size),
        },
    };

    raw_info.into()
}

impl SymbolicationActor {
    async fn parse_apple_crash_report(
        &self,
        scope: Scope,
        minidump: Bytes,
        sources: Vec<SourceConfig>,
        options: RequestOptions,
    ) -> Result<(SymbolicateStacktraces, AppleCrashReportState), SymbolicationError> {
        let parse_future = async {
            let report = AppleCrashReport::from_reader(minidump.into_buf())?;
            let mut metadata = report.metadata;

            let arch = report
                .code_type
                .as_ref()
                .and_then(|code_type| code_type.split(' ').next())
                .and_then(|word| word.parse().ok())
                .unwrap_or_default();

            let modules = report
                .binary_images
                .into_iter()
                .map(map_apple_binary_image)
                .collect();

            let mut stacktraces = Vec::with_capacity(report.threads.len());

            for thread in report.threads {
                let registers = thread
                    .registers
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, addr)| (name, HexValue(addr.0)))
                    .collect();

                let frames = thread
                    .frames
                    .into_iter()
                    .map(|frame| RawFrame {
                        instruction_addr: HexValue(frame.instruction_addr.0),
                        package: frame.module,
                        ..RawFrame::default()
                    })
                    .collect();

                stacktraces.push(RawStacktrace {
                    thread_id: Some(thread.id),
                    is_requesting: Some(thread.crashed),
                    registers,
                    frames,
                });
            }

            let request = SymbolicateStacktraces {
                modules,
                scope,
                sources: Arc::from(sources),
                signal: None,
                stacktraces,
                options,
            };

            let mut system_info = SystemInfo {
                os_name: metadata.remove("OS Version").unwrap_or_default(),
                device_model: metadata.remove("Hardware Model").unwrap_or_default(),
                cpu_arch: arch,
                ..SystemInfo::default()
            };

            if let Some(captures) = OS_MACOS_REGEX.captures(&system_info.os_name) {
                system_info.os_version = captures
                    .name("version")
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                system_info.os_build = captures
                    .name("build")
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                system_info.os_name = "macOS".to_string();
            }

            // https://developer.apple.com/library/archive/technotes/tn2151/_index.html
            let crash_reason = metadata.remove("Exception Type");
            let crash_details = report
                .application_specific_information
                .or_else(|| metadata.remove("Exception Message"))
                .or_else(|| metadata.remove("Exception Subtype"))
                .or_else(|| metadata.remove("Exception Codes"));

            let state = AppleCrashReportState {
                timestamp: report.timestamp,
                system_info,
                crash_reason,
                crash_details,
            };

            Ok((request, state))
        };

        let future = async move {
            self.threadpool
                .spawn_handle(parse_future.bind_hub(sentry::Hub::current()))
                .await
                .context("Parse applecrashreport future cancelled")
        };

        let future = timeout_compat(Duration::from_secs(1200), future);
        let future = measure("parse_apple_crash_report", m::timed_result, future);
        future
            .await
            .map(|res| res.map_err(SymbolicationError::from))
            .unwrap_or(Err(SymbolicationError::Timeout))?
    }

    async fn do_process_apple_crash_report(
        self,
        scope: Scope,
        report: Bytes,
        sources: Vec<SourceConfig>,
        options: RequestOptions,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let (request, state) = self
            .parse_apple_crash_report(scope, report, sources, options)
            .await?;
        let mut response = self.do_symbolicate(request).await?;

        state.merge_into(&mut response);
        Ok(response)
    }

    pub fn process_apple_crash_report(
        &self,
        scope: Scope,
        apple_crash_report: Bytes,
        sources: Vec<SourceConfig>,
        options: RequestOptions,
    ) -> RequestId {
        self.create_symbolication_request(self.clone().do_process_apple_crash_report(
            scope,
            apple_crash_report,
            sources,
            options,
        ))
    }
}

fn map_symbolic_registers(x: BTreeMap<&'_ str, RegVal>) -> BTreeMap<String, HexValue> {
    x.into_iter()
        .map(|(register, value)| {
            (
                register.to_owned(),
                HexValue(match value {
                    RegVal::U32(x) => x.into(),
                    RegVal::U64(x) => x,
                }),
            )
        })
        .collect()
}

impl From<&CfiCacheError> for ObjectFileStatus {
    fn from(e: &CfiCacheError) -> ObjectFileStatus {
        match e {
            CfiCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            CfiCacheError::Timeout => ObjectFileStatus::Timeout,
            CfiCacheError::ObjectParsing(_) => ObjectFileStatus::Malformed,

            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

impl From<&SymCacheError> for ObjectFileStatus {
    fn from(e: &SymCacheError) -> ObjectFileStatus {
        match e {
            SymCacheError::Fetching(_) => ObjectFileStatus::FetchingFailed,
            // nb: Timeouts during download are also caught by Fetching
            SymCacheError::Timeout => ObjectFileStatus::Timeout,
            SymCacheError::Malformed => ObjectFileStatus::Malformed,
            SymCacheError::ObjectParsing(_) => ObjectFileStatus::Malformed,
            _ => {
                // Just in case we didn't handle an error properly,
                // capture it here. If an error was captured with
                // `capture_error` further down in the callstack, it
                // should be explicitly handled here as a
                // SymCacheError variant.
                sentry::capture_error(e);
                ObjectFileStatus::Other
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use crate::app::ServiceState;
    use crate::config::Config;
    use crate::test;

    /// Setup tests and create a test service.
    ///
    /// This function returns a tuple containing the service to test, and a temporary cache
    /// directory. The directory is cleaned up when the [`TempDir`] instance is dropped. Keep it as
    /// guard until the test has finished.
    ///
    /// The service is configured with `connect_to_reserved_ips = True`. This allows to use a local
    /// symbol server to test object file downloads.
    fn setup_service() -> (ServiceState, test::TempDir) {
        test::setup();

        let cache_dir = test::tempdir();

        let config = Config {
            cache_dir: Some(cache_dir.path().to_owned()),
            connect_to_reserved_ips: true,
            ..Default::default()
        };
        let service = ServiceState::create(config).unwrap();

        (service, cache_dir)
    }

    fn get_symbolication_request(sources: Vec<SourceConfig>) -> SymbolicateStacktraces {
        SymbolicateStacktraces {
            scope: Scope::Global,
            signal: None,
            sources: Arc::from(sources),
            stacktraces: vec![RawStacktrace {
                frames: vec![RawFrame {
                    instruction_addr: HexValue(0x1_0000_0fa0),
                    ..RawFrame::default()
                }],
                ..RawStacktrace::default()
            }],
            modules: vec![CompleteObjectInfo::from(RawObjectInfo {
                ty: ObjectType::Macho,
                code_id: Some("502fc0a51ec13e479998684fa139dca7".to_owned().to_lowercase()),
                debug_id: Some("502fc0a5-1ec1-3e47-9998-684fa139dca7".to_owned()),
                image_addr: HexValue(0x1_0000_0000),
                image_size: Some(4096),
                code_file: None,
                debug_file: None,
            })],
            options: RequestOptions {
                dif_candidates: true,
            },
        }
    }

    /// Helper to redact the port number from localhost URIs in insta snapshots.
    ///
    /// Since we use a localhost source on a random port during tests we get random port
    /// numbers in URI of the dif object file candidates.  This redaction masks this out.
    fn redact_localhost_port(
        value: insta::internals::Content,
        _path: insta::internals::ContentPath<'_>,
    ) -> impl Into<insta::internals::Content> {
        let re = regex::Regex::new(r"^http://localhost:[0-9]+").unwrap();
        re.replace(value.as_str().unwrap(), "http://localhost:<port>")
            .into_owned()
    }

    macro_rules! assert_snapshot {
        ($e:expr) => {
            ::insta::assert_yaml_snapshot!($e, {
                ".**.location" => ::insta::dynamic_redaction(
                    $crate::actors::symbolication::tests::redact_localhost_port
                )
            });
        }
    }

    #[tokio::test]
    async fn test_remove_bucket() -> Result<(), SymbolicationError> {
        // Test with sources first, and then without. This test should verify that we do not leak
        // cached debug files to requests that no longer specify a source.

        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let symbolication = service.symbolication();
        let response = test::spawn_compat(move || async move {
            let request = get_symbolication_request(vec![source]);
            let request_id = symbolication.symbolicate_stacktraces(request);
            symbolication.get_response(request_id, None).await
        });

        assert_snapshot!(response.await.unwrap());

        let symbolication = service.symbolication();
        let response = test::spawn_compat(move || async move {
            let request = get_symbolication_request(vec![]);
            let request_id = symbolication.symbolicate_stacktraces(request);
            symbolication.get_response(request_id, None).await
        });

        assert_snapshot!(response.await.unwrap());

        Ok(())
    }

    #[tokio::test]
    async fn test_add_bucket() -> anyhow::Result<()> {
        // Test without sources first, then with. This test should verify that we apply a new source
        // to requests immediately.

        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let symbolication = service.symbolication();
        let response = test::spawn_compat(move || async move {
            let request = get_symbolication_request(vec![]);
            let request_id = symbolication.symbolicate_stacktraces(request);
            symbolication.get_response(request_id, None).await
        });

        assert_snapshot!(response.await.unwrap());

        let symbolication = service.symbolication();
        let response = test::spawn_compat(move || async move {
            let request = get_symbolication_request(vec![source]);
            let request_id = symbolication.symbolicate_stacktraces(request);
            symbolication.get_response(request_id, None).await
        });

        assert_snapshot!(response.await.unwrap());

        Ok(())
    }

    #[tokio::test]
    async fn test_get_response_multi() {
        // Make sure we can repeatedly poll for the response
        let (service, _cache_dir) = setup_service();

        let stacktraces = serde_json::from_str(
            r#"[
              {
                "frames":[
                  {
                    "instruction_addr":"0x8c",
                    "addr_mode":"rel:0"
                  }
                ]
              }
            ]"#,
        )
        .unwrap();

        let request = SymbolicateStacktraces {
            modules: Vec::new(),
            stacktraces,
            signal: None,
            sources: Arc::new([]),
            scope: Default::default(),
            options: Default::default(),
        };

        test::spawn_compat(move || async move {
            // Be aware, this spawns the work into a new current thread runtime, which gets
            // dropped when test::spawn_compat() returns.
            let request_id = service.symbolication().symbolicate_stacktraces(request);

            for _ in 0..2 {
                let response = service
                    .symbolication()
                    .get_response(request_id, None)
                    .await
                    .unwrap();

                if !matches!(&response, SymbolicationResponse::Completed(_)) {
                    panic!("Not a complete response: {:#?}", response);
                }
            }
        })
        .await;
    }

    async fn stackwalk_minidump(path: &str) -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let minidump = Bytes::from(fs::read(path)?);
        let symbolication = service.symbolication();
        let response = test::spawn_compat(move || async move {
            let request_id = symbolication.process_minidump(
                Scope::Global,
                minidump,
                vec![source],
                RequestOptions {
                    dif_candidates: true,
                },
            );
            symbolication.get_response(request_id, None).await
        });

        assert_snapshot!(response.await.unwrap());

        let global_dir = service.config().cache_dir("object_meta/global").unwrap();
        let mut cache_entries: Vec<_> = fs::read_dir(global_dir)?
            .map(|x| x.unwrap().file_name().into_string().unwrap())
            .collect();

        cache_entries.sort();
        assert_snapshot!(cache_entries);

        Ok(())
    }

    #[tokio::test]
    async fn test_minidump_windows() -> anyhow::Result<()> {
        stackwalk_minidump("./tests/fixtures/windows.dmp").await
    }

    #[tokio::test]
    async fn test_minidump_macos() -> anyhow::Result<()> {
        stackwalk_minidump("./tests/fixtures/macos.dmp").await
    }

    #[tokio::test]
    async fn test_minidump_linux() -> anyhow::Result<()> {
        stackwalk_minidump("./tests/fixtures/linux.dmp").await
    }

    #[tokio::test]
    async fn test_apple_crash_report() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let report_file = Bytes::from(fs::read("./tests/fixtures/apple_crash_report.txt")?);
        let response = test::spawn_compat(move || async move {
            let request_id = service.symbolication().process_apple_crash_report(
                Scope::Global,
                report_file,
                vec![source],
                RequestOptions {
                    dif_candidates: true,
                },
            );

            service.symbolication().get_response(request_id, None).await
        });

        assert_snapshot!(response.await.unwrap());
        Ok(())
    }

    #[tokio::test]
    async fn test_wasm_payload() -> anyhow::Result<()> {
        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let modules: Vec<RawObjectInfo> = serde_json::from_str(
            r#"[
              {
                "type":"wasm",
                "debug_id":"bda18fd8-5d4a-4eb8-9302-2d6bfad846b1",
                "code_id":"bda18fd85d4a4eb893022d6bfad846b1",
                "debug_file":"file://foo.invalid/demo.wasm"
              }
            ]"#,
        )?;

        let stacktraces = serde_json::from_str(
            r#"[
              {
                "frames":[
                  {
                    "instruction_addr":"0x8c",
                    "addr_mode":"rel:0"
                  }
                ]
              }
            ]"#,
        )?;

        let request = SymbolicateStacktraces {
            modules: modules.into_iter().map(From::from).collect(),
            stacktraces,
            signal: None,
            sources: Arc::new([source]),
            scope: Default::default(),
            options: Default::default(),
        };

        let response = test::spawn_compat(move || async move {
            let request_id = service.symbolication().symbolicate_stacktraces(request);
            service.symbolication().get_response(request_id, None).await
        });

        insta::assert_yaml_snapshot!(response.await.unwrap());
        Ok(())
    }

    #[test]
    fn test_symcache_lookup_open_end_addr() {
        test::setup();

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
        });

        let lookup = SymCacheLookup::from_iter(vec![info.clone()]);

        let lookup_result = lookup.lookup_symcache(43, AddrMode::Abs).unwrap();
        assert_eq!(lookup_result.module_index, 0);
        assert_eq!(lookup_result.object_info, &info);
        assert!(lookup_result.symcache.is_none());
    }

    fn create_object_info(has_id: bool, addr: u64, size: Option<u64>) -> CompleteObjectInfo {
        RawObjectInfo {
            ty: ObjectType::Elf,
            code_id: None,
            code_file: None,
            debug_id: Some(uuid::Uuid::new_v4().to_string()).filter(|_| has_id),
            debug_file: None,
            image_addr: HexValue(addr),
            image_size: size,
        }
        .into()
    }

    #[test]
    fn test_code_module_builder_empty() {
        let modules = vec![];

        let valid = CodeModulesBuilder::new(modules.clone()).build();
        assert_eq!(valid, modules);
    }

    #[test]
    fn test_code_module_builder_valid() {
        let modules = vec![
            create_object_info(true, 0x1000, Some(0x1000)),
            create_object_info(true, 0x3000, Some(0x1000)),
        ];

        let valid = CodeModulesBuilder::new(modules.clone()).build();
        assert_eq!(valid, modules);
    }

    #[test]
    fn test_code_module_builder_unreferenced() {
        let valid_object = create_object_info(true, 0x1000, Some(0x1000));
        let modules = vec![
            valid_object.clone(),
            create_object_info(false, 0x3000, Some(0x1000)),
        ];

        let valid = CodeModulesBuilder::new(modules).build();
        assert_eq!(valid, vec![valid_object]);
    }

    #[test]
    fn test_code_module_builder_referenced() {
        let modules = vec![
            create_object_info(true, 0x1000, Some(0x1000)),
            create_object_info(false, 0x3000, Some(0x1000)),
        ];

        let mut builder = CodeModulesBuilder::new(modules.clone());
        builder.mark_referenced(0x3500);
        let valid = builder.build();
        assert_eq!(valid, modules);
    }

    #[test]
    fn test_code_module_builder_miss_first() {
        let modules = vec![
            create_object_info(false, 0x1000, Some(0x1000)),
            create_object_info(false, 0x3000, Some(0x1000)),
        ];

        let mut builder = CodeModulesBuilder::new(modules);
        builder.mark_referenced(0xfff);
        let valid = builder.build();
        assert_eq!(valid, vec![]);
    }

    #[test]
    fn test_code_module_builder_gap() {
        let modules = vec![
            create_object_info(false, 0x1000, Some(0x1000)),
            create_object_info(false, 0x3000, Some(0x1000)),
        ];

        let mut builder = CodeModulesBuilder::new(modules);
        builder.mark_referenced(0x2800); // in the gap between both modules
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

        let mut builder = CodeModulesBuilder::new(modules);
        builder.mark_referenced(0x2800); // in the gap between both modules
        let valid = builder.build();
        assert_eq!(valid, vec![valid_object]);
    }
}
