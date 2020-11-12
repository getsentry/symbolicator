use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;
use std::io::Write;
use std::iter::FromIterator;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use actix::ResponseFuture;
use apple_crash_report_parser::AppleCrashReport;
use bytes::{Bytes, IntoBuf};
use chrono::{DateTime, TimeZone, Utc};
use futures::{compat::Future01CompatExt, FutureExt as _, TryFutureExt};
use futures01::future::{self, join_all, Future, IntoFuture, Shared};
use futures01::sync::oneshot;
use parking_lot::Mutex;
use regex::Regex;
use sentry::SentryFutureExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use symbolic::common::{
    Arch, ByteView, CodeId, DebugId, InstructionInfo, Language, Name, SelfCell,
};
use symbolic::debuginfo::{Object, ObjectDebugSession};
use symbolic::demangle::{Demangle, DemangleFormat, DemangleOptions};
use symbolic::minidump::cfi::CfiCache;
use symbolic::minidump::processor::{
    CodeModule, CodeModuleId, FrameTrust, ProcessMinidumpError, ProcessState, RegVal,
};
use thiserror::Error;
use tokio::timer::Delay;

use crate::actors::cficaches::{CfiCacheActor, CfiCacheError, CfiCacheFile, FetchCfiCache};
use crate::actors::objects::{FindObject, ObjectError, ObjectPurpose, ObjectsActor};
use crate::actors::symcaches::{FetchSymCache, SymCacheActor, SymCacheError, SymCacheFile};
use crate::cache::CacheStatus;
use crate::logging::LogError;
use crate::sources::{FileType, SourceConfig};
use crate::types::{
    CompleteObjectInfo, CompleteStacktrace, CompletedSymbolicationResponse, FrameStatus,
    ObjectFileStatus, ObjectId, ObjectType, RawFrame, RawObjectInfo, RawStacktrace, Registers,
    RequestId, Scope, Signal, SymbolicatedFrame, SymbolicationResponse, SystemInfo,
};
use crate::utils::futures::{CallOnDrop, ThreadPool};
use crate::utils::hex::HexValue;
use crate::utils::sentry::SentryFutureExt as SentryLegacyFutureExt;

/// Options for demangling all symbols.
const DEMANGLE_OPTIONS: DemangleOptions = DemangleOptions {
    with_arguments: true,
    format: DemangleFormat::Short,
};

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

    #[error("internal IO failed")]
    Io(#[from] std::io::Error),

    #[error("computation was canceled internally")]
    Canceled,

    #[error("failed to process minidump")]
    InvalidMinidump(#[from] ProcessMinidumpError),

    #[error("failed to parse apple crash report")]
    InvalidAppleCrashReport(#[from] apple_crash_report_parser::ParseError),
}

impl From<&SymbolicationError> for SymbolicationResponse {
    fn from(err: &SymbolicationError) -> SymbolicationResponse {
        match err {
            SymbolicationError::Timeout => SymbolicationResponse::Timeout,
            SymbolicationError::Io(_) => SymbolicationResponse::InternalError,
            SymbolicationError::Canceled => SymbolicationResponse::InternalError,
            SymbolicationError::InvalidMinidump(_) => SymbolicationResponse::Failed {
                message: err.to_string(),
            },
            SymbolicationError::InvalidAppleCrashReport(_) => SymbolicationResponse::Failed {
                message: err.to_string(),
            },
        }
    }
}

// We want a shared future here because otherwise polling for a response would hold the global lock.
type ComputationChannel = Shared<oneshot::Receiver<(Instant, SymbolicationResponse)>>;

type ComputationMap = Arc<Mutex<BTreeMap<RequestId, ComputationChannel>>>;

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
        let requests = Arc::new(Mutex::new(BTreeMap::new()));

        SymbolicationActor {
            objects,
            symcaches,
            cficaches,
            diagnostics_cache,
            threadpool,
            requests,
            spawnpool: Arc::new(spawnpool),
        }
    }

    fn wrap_response_channel(
        &self,
        request_id: RequestId,
        timeout: Option<u64>,
        channel: ComputationChannel,
    ) -> ResponseFuture<SymbolicationResponse, SymbolicationError> {
        let rv = channel
            .map(|item| (*item).clone())
            .map_err(|_| SymbolicationError::Canceled);

        if let Some(timeout) = timeout.map(Duration::from_secs) {
            Box::new(tokio::timer::Timeout::new(rv, timeout).then(move |result| {
                match result {
                    Ok((finished_at, response)) => {
                        metric!(timer("requests.response_idling") = finished_at.elapsed());
                        Ok(response)
                    }
                    Err(timeout_error) => match timeout_error.into_inner() {
                        Some(error) => Err(error),
                        None => Ok(SymbolicationResponse::Pending {
                            request_id,
                            // XXX(markus): Probably need a better estimation at some
                            // point.
                            retry_after: 30,
                        }),
                    },
                }
            }))
        } else {
            Box::new(rv.then(move |result| {
                let (finished_at, response) = result?;
                metric!(timer("requests.response_idling") = finished_at.elapsed());
                Ok(response)
            }))
        }
    }

    fn create_symbolication_request<F>(&self, f: F) -> RequestId
    where
        F: std::future::Future<Output = Result<CompletedSymbolicationResponse, SymbolicationError>>
            + 'static,
    {
        let (sender, receiver) = oneshot::channel();

        // Assume that there are no UUID4 collisions in practice.
        let requests = self.requests.clone();
        let request_id = RequestId::new(uuid::Uuid::new_v4());
        requests.lock().insert(request_id, receiver.shared());
        let token = CallOnDrop::new(move || {
            requests.lock().remove(&request_id);
        });

        let request_future = async move {
            let response = match f.await {
                Ok(response) => SymbolicationResponse::Completed(Box::new(response)),
                Err(ref error) => {
                    sentry::capture_error(error);
                    error.into()
                }
            };

            sender.send((Instant::now(), response)).ok();

            // Wait before removing the channel from the computation map to allow clients to
            // poll the status.
            Delay::new(Instant::now() + MAX_POLL_DELAY)
                .compat()
                .await
                .ok();
            drop(token);
            Ok(())
        }
        .bind_hub(sentry::Hub::new_from_top(sentry::Hub::current()))
        .boxed_local()
        .compat();

        // TODO: This spawns into the arbiter of the caller, which usually is the web handler. This
        // doesn't block the web request, but it congests the threads that should only do web I/O.
        // Instead, this should spawn into a dedicated resource (e.g. a threadpool) to keep web
        // requests flowing while symbolication tasks may backlog.
        actix::spawn(request_future);

        request_id
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
        debug_id: Some(module.debug_identifier()),
        debug_file: Some(module.debug_file()),
        image_addr: HexValue(module.base_address()),
        image_size: match module.size() {
            0 => None,
            size => Some(size),
        },
    }
}

pub struct SourceObject(SelfCell<ByteView<'static>, Object<'static>>);

struct SourceLookup {
    inner: Vec<(CompleteObjectInfo, Option<Arc<SourceObject>>)>,
}

impl SourceLookup {
    pub async fn fetch_sources(
        self,
        objects: ObjectsActor,
        scope: Scope,
        sources: Arc<Vec<SourceConfig>>,
        response: &CompletedSymbolicationResponse,
    ) -> Result<Self, SymbolicationError> {
        let mut referenced_objects = BTreeSet::new();
        let stacktraces = &response.stacktraces;

        for stacktrace in stacktraces {
            for frame in &stacktrace.frames {
                if let Some(i) = self.get_object_index_by_addr(frame.raw.instruction_addr.0) {
                    referenced_objects.insert(i);
                }
            }
        }

        let mut futures = Vec::new();

        for (i, (mut object_info, _)) in self.inner.into_iter().enumerate() {
            let is_used = referenced_objects.contains(&i);
            let objects = objects.clone();
            let scope = scope.clone();
            let sources = sources.clone();

            futures.push(async move {
                if !is_used {
                    object_info.debug_status = ObjectFileStatus::Unused;
                    return (object_info, None);
                }

                let opt_object_file_meta = objects
                    .find(FindObject {
                        filetypes: FileType::sources(),
                        purpose: ObjectPurpose::Source,
                        scope: scope.clone(),
                        identifier: object_id_from_object_info(&object_info.raw),
                        sources,
                    })
                    .compat()
                    .await
                    .unwrap_or(None);

                let object_file_opt = match opt_object_file_meta {
                    None => None,
                    Some(object_file_meta) => objects
                        .fetch(object_file_meta)
                        .compat()
                        .await
                        .ok()
                        .and_then(|x| {
                            SelfCell::try_new(x.data(), |b| Object::parse(unsafe { &*b }))
                                .map(|x| Arc::new(SourceObject(x)))
                                .ok()
                        }),
                };

                if object_file_opt.is_some() {
                    object_info.features.has_sources = true;
                }

                (object_info, object_file_opt)
            });
        }

        Ok(SourceLookup {
            inner: futures::future::join_all(futures).await,
        })
    }

    pub fn prepare_debug_sessions(&self) -> Vec<Option<ObjectDebugSession<'_>>> {
        self.inner
            .iter()
            .map(|&(_, ref o)| o.as_ref().and_then(|o| o.0.get().debug_session().ok()))
            .collect()
    }

    pub fn get_context_lines(
        &self,
        debug_sessions: &[Option<ObjectDebugSession<'_>>],
        addr: u64,
        abs_path: &str,
        lineno: u32,
        n: usize,
    ) -> Option<(Vec<String>, String, Vec<String>)> {
        let index = self.get_object_index_by_addr(addr)?;
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

    fn get_object_index_by_addr(&self, addr: u64) -> Option<usize> {
        for (i, (info, _)) in self.inner.iter().enumerate() {
            let start_addr = info.raw.image_addr.0;

            if start_addr > addr {
                // The debug image starts at a too high address
                continue;
            }

            let size = info.raw.image_size.unwrap_or(0);
            if let Some(end_addr) = start_addr.checked_add(size) {
                if end_addr < addr && size != 0 {
                    // The debug image ends at a too low address and we're also confident that
                    // end_addr is accurate (size != 0)
                    continue;
                }
            }

            return Some(i);
        }

        None
    }

    fn sort(&mut self) {
        self.inner.sort_by_key(|(info, _)| info.raw.image_addr.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        self.inner.dedup_by(|(ref info2, _), (ref mut info1, _)| {
            // If this underflows we didn't sort properly.
            let size = info2.raw.image_addr.0 - info1.raw.image_addr.0;
            if info1.raw.image_size.unwrap_or(0) == 0 {
                info1.raw.image_size = Some(size);
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
            inner: iter.into_iter().map(|x| (x, None)).collect(),
        };
        rv.sort();
        rv
    }
}

struct SymCacheLookup {
    inner: Vec<(CompleteObjectInfo, Option<Arc<SymCacheFile>>)>,
}

impl FromIterator<CompleteObjectInfo> for SymCacheLookup {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = CompleteObjectInfo>,
    {
        let mut rv = SymCacheLookup {
            inner: iter.into_iter().map(|x| (x, None)).collect(),
        };
        rv.sort();
        rv
    }
}

impl SymCacheLookup {
    fn sort(&mut self) {
        self.inner.sort_by_key(|(info, _)| info.raw.image_addr.0);

        // Ignore the name `dedup_by`, I just want to iterate over consecutive items and update
        // some.
        self.inner.dedup_by(|(ref info2, _), (ref mut info1, _)| {
            // If this underflows we didn't sort properly.
            let size = info2.raw.image_addr.0 - info1.raw.image_addr.0;
            if info1.raw.image_size.unwrap_or(0) == 0 {
                info1.raw.image_size = Some(size);
            }

            false
        });
    }

    async fn fetch_symcaches(
        self,
        symcache_actor: SymCacheActor,
        request: SymbolicateStacktraces,
    ) -> Result<Self, SymbolicationError> {
        let mut referenced_objects = BTreeSet::new();
        let stacktraces = request.stacktraces;

        for stacktrace in stacktraces {
            for frame in stacktrace.frames {
                if let Some((i, ..)) = self.lookup_symcache(frame.instruction_addr.0) {
                    referenced_objects.insert(i);
                }
            }
        }

        let mut futures = Vec::new();

        for (i, (mut object_info, _)) in self.inner.into_iter().enumerate() {
            let is_used = referenced_objects.contains(&i);
            let sources = request.sources.clone();
            let scope = request.scope.clone();
            let symcache_actor = symcache_actor.clone();

            futures.push(async move {
                if !is_used {
                    object_info.debug_status = ObjectFileStatus::Unused;
                    return (object_info, None);
                }
                let symcache_result = symcache_actor
                    .fetch(FetchSymCache {
                        object_type: object_info.raw.ty,
                        identifier: object_id_from_object_info(&object_info.raw),
                        sources,
                        scope,
                    })
                    .compat()
                    .await;

                let (symcache, status) = match symcache_result {
                    Ok(symcache) => match symcache.parse() {
                        Ok(Some(_)) => (Some(symcache), ObjectFileStatus::Found),
                        Ok(None) => (Some(symcache), ObjectFileStatus::Missing),
                        Err(e) => (None, (&e).into()),
                    },
                    Err(e) => (None, (&*e).into()),
                };

                object_info.arch = Default::default();

                if let Some(ref symcache) = symcache {
                    object_info.arch = symcache.arch();
                    object_info.features.merge(symcache.features());
                }

                object_info.debug_status = status;
                (object_info, symcache)
            });
        }

        Ok(SymCacheLookup {
            inner: futures::future::join_all(futures).await,
        })
    }

    fn lookup_symcache(
        &self,
        addr: u64,
    ) -> Option<(usize, &CompleteObjectInfo, Option<&SymCacheFile>)> {
        for (i, (info, cache)) in self.inner.iter().enumerate() {
            let start_addr = info.raw.image_addr.0;

            if start_addr > addr {
                // The debug image starts at a too high address
                continue;
            }

            let size = info.raw.image_size.unwrap_or(0);
            if let Some(end_addr) = start_addr.checked_add(size) {
                if end_addr < addr && size != 0 {
                    // The debug image ends at a too low address and we're also confident that
                    // end_addr is accurate (size != 0)
                    continue;
                }
            }

            return Some((i, info, cache.as_ref().map(|x| &**x)));
        }

        None
    }
}

fn symbolicate_frame(
    caches: &SymCacheLookup,
    registers: &Registers,
    signal: Option<Signal>,
    frame: &mut RawFrame,
    index: usize,
) -> Result<Vec<SymbolicatedFrame>, FrameStatus> {
    let (object_info, symcache) = match caches.lookup_symcache(frame.instruction_addr.0) {
        Some((_, info, Some(symcache))) => {
            frame.package = info.raw.code_file.clone();
            (info, symcache)
        }
        Some((_, info, None)) => {
            frame.package = info.raw.code_file.clone();
            if info.debug_status == ObjectFileStatus::Malformed {
                return Err(FrameStatus::Malformed);
            } else {
                return Err(FrameStatus::Missing);
            }
        }
        None => return Err(FrameStatus::UnknownImage),
    };

    log::trace!("Loading symcache");
    let symcache = match symcache.parse() {
        Ok(Some(x)) => x,
        Ok(None) => return Err(FrameStatus::Missing),
        Err(_) => return Err(FrameStatus::Malformed),
    };

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

    let caller_address = InstructionInfo::new(symcache.arch(), frame.instruction_addr.0)
        .is_crashing_frame(is_crashing_frame)
        .signal(signal.map(|signal| signal.0))
        .ip_register_value(ip_register_value)
        .caller_address();

    let relative_addr = match caller_address.checked_sub(object_info.raw.image_addr.0) {
        Some(x) => x,
        None => {
            log::warn!("Underflow when trying to subtract image start addr from caller address");
            metric!(counter("relative_addr.underflow") += 1);
            return Err(FrameStatus::MissingSymbol);
        }
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
        let detected_language = Name::new(name.as_str()).detect_language();
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
                package: object_info.raw.code_file.clone(),
                instruction_addr: HexValue(
                    object_info.raw.image_addr.0 + line_info.instruction_address(),
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
                    object_info.raw.image_addr.0 + line_info.function_address(),
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
                let demangled = mangled.and_then(|m| Name::new(m).demangle(DEMANGLE_OPTIONS));
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

#[derive(Debug, Clone, Deserialize)]
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
    pub sources: Arc<Vec<SourceConfig>>,

    /// A list of threads containing stack traces.
    pub stacktraces: Vec<RawStacktrace>,

    /// A list of images that were loaded into the process.
    ///
    /// This list must cover the instruction addresses of the frames in `threads`. If a frame is not
    /// covered by any image, the frame cannot be symbolicated as it is not clear which debug file
    /// to load.
    pub modules: Vec<CompleteObjectInfo>,
}

impl SymbolicationActor {
    async fn do_symbolicate(
        self,
        request: SymbolicateStacktraces,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let result = self.do_symbolicate_impl(request).boxed_local().compat();

        future_metrics!(
            "symbolicate",
            Some((Duration::from_secs(3600), SymbolicationError::Timeout)),
            result
        )
        .compat()
        .await
    }

    async fn do_symbolicate_impl(
        self,
        request: SymbolicateStacktraces,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let symcache_lookup: SymCacheLookup = request.modules.iter().cloned().collect();
        let source_lookup: SourceLookup = request.modules.iter().cloned().collect();
        let stacktraces = request.stacktraces.clone();
        let sources = request.sources.clone();
        let scope = request.scope.clone();
        let signal = request.signal;

        let symcache_lookup = symcache_lookup
            .fetch_symcaches(self.symcaches, request)
            .await?;

        let future = async move {
            let stacktraces: Vec<_> = stacktraces
                .into_iter()
                .map(|trace| symbolicate_stacktrace(trace, &symcache_lookup, signal))
                .collect();

            let modules: Vec<_> = symcache_lookup
                .inner
                .into_iter()
                .map(|(object_info, _)| {
                    metric!(
                        counter("symbolication.debug_status") += 1,
                        "status" => object_info.debug_status.name()
                    );

                    object_info
                })
                .collect();

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
            .map_err(|_| SymbolicationError::Canceled)?;

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

        Ok(self
            .threadpool
            .spawn_handle(future.bind_hub(sentry::Hub::current()))
            .await
            .map_err(|_| SymbolicationError::Canceled)?)
    }

    pub fn symbolicate_stacktraces(&self, request: SymbolicateStacktraces) -> RequestId {
        self.create_symbolication_request(self.clone().do_symbolicate(request))
    }

    /// Polls the status for a started symbolication task.
    ///
    /// If the timeout is set and no result is ready within the given time, a `pending` status is
    pub fn get_response(
        &self,
        request_id: RequestId,
        timeout: Option<u64>,
    ) -> ResponseFuture<Option<SymbolicationResponse>, SymbolicationError> {
        let channel_opt = self.requests.lock().get(&request_id).cloned();
        match channel_opt {
            Some(channel) => Box::new(
                self.wrap_response_channel(request_id, timeout, channel)
                    .map(Some),
            ),
            None => {
                // This is okay to occur during deploys, but if it happens all the time we have a state
                // bug somewhere. Could be a misconfigured load balancer (supposed to be pinned to
                // scopes).
                metric!(counter("symbolication.request_id_unknown") += 1);
                Box::new(future::ok(None))
            }
        }
    }
}

type CfiCacheResult = (CodeModuleId, Result<Arc<CfiCacheFile>, Arc<CfiCacheError>>);

/// Contains some meta-data about a minidump.
///
/// The minidump meta-data contained here is extracted in a [procspawn] subprocess so needs
/// to be (de)serialisable to/from JSON.  It is only a way to get this metadata out of the
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
    ) -> Result<Vec<(CodeModuleId, RawObjectInfo)>, SymbolicationError> {
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
            .map_err(|_| SymbolicationError::Canceled)?
    }

    /// Join a procspawn handle with a timeout.
    ///
    /// This handles the procspawn result, makes sure to appropriately log any failures and
    /// save the minidump for debugging.  Returns a simple result converted to the
    /// `SymbolicationError`.
    fn join_procspawn<T, E>(
        handle: procspawn::JoinHandle<Result<procspawn::serde::Json<T>, E>>,
        timeout: Duration,
        metric: &str,
        minidump: Bytes,
        minidump_cache: crate::cache::Cache,
    ) -> Result<T, SymbolicationError>
    where
        T: Serialize + DeserializeOwned,
        E: Into<SymbolicationError> + Serialize + DeserializeOwned,
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
                let kind = if perr.is_timeout() {
                    SymbolicationError::Timeout
                } else {
                    SymbolicationError::Canceled
                };
                metric!(counter(metric) += 1, "reason" => reason);
                if let SymbolicationError::Canceled = kind {
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
                Err(kind)
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
        sources: Arc<Vec<SourceConfig>>,
    ) -> Result<Vec<CfiCacheResult>, SymbolicationError> {
        let cficaches = self.cficaches.clone();

        let futures = requests
            .into_iter()
            .map(move |(code_module_id, object_info)| {
                cficaches
                    .fetch(FetchCfiCache {
                        object_type: object_info.ty,
                        identifier: object_id_from_object_info(&object_info),
                        sources: sources.clone(),
                        scope: scope.clone(),
                    })
                    .then(move |result| Ok((code_module_id, result)).into_future())
                    // Clone hub because of join_all
                    .sentry_hub_new_from_current()
            });

        join_all(futures).compat().await
    }

    /// Unwind the stack from a minidump.
    ///
    /// This processes the minidump to stackwalk all the threads found in the minidump.
    ///
    /// The `cfi_results` must contain all modules found in the minidump (extracted using
    /// [SymbolicationActor::get_referenced_modules_from_minidump]) and the result of trying
    /// to fetch the Call Frame Information (CFI) for them from the [CfiCacheActor].
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
        sources: Arc<Vec<SourceConfig>>,
        cfi_results: Vec<CfiCacheResult>,
    ) -> Result<(SymbolicateStacktraces, MinidumpState), SymbolicationError> {
        let mut unwind_statuses = BTreeMap::new();
        let mut object_features = BTreeMap::new();
        let mut frame_info_map = BTreeMap::new();

        // Go through all the modules in the minidump and build a map of the modules with
        // missing or malformed CFI.  ObjectFileStatus::Found is only added when the file is
        // loaded as it could still be corrupted when reading from the cache.  Usable CFI
        // cache files are added to the frame_info_map.
        for (code_module_id, result) in &cfi_results {
            let cache_file = match result {
                Ok(x) => x,
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
                    ),
                    |(
                        frame_info_map,
                        object_features,
                        mut unwind_statuses,
                        minidump,
                        spawn_time,
                    )|
                     -> Result<_, ProcessMinidumpError> {
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
                                    log::warn!("Error while parsing cficache: {}", LogError(&e));
                                    unwind_statuses.insert(code_module_id, (&e).into());
                                }
                            }
                        }

                        // Stackwalk the minidump.
                        let minidump = ByteView::from_slice(&minidump);
                        let process_state = ProcessState::from_minidump(&minidump, Some(&cfi))?;

                        let minidump_state = MinidumpState::new(&process_state);
                        let object_type = minidump_state.object_type();

                        // Start building the module list to be returned in the
                        // symbolication response.  For all modules in the minidump we
                        // create the CompletedObjectInfo and start populating it.
                        let mut module_builder = process_state
                            .modules()
                            .into_iter()
                            .map(|code_module| {
                                let mut info: CompleteObjectInfo =
                                    object_info_from_minidump_module(object_type, code_module)
                                        .into();

                                let module_id = code_module.id();

                                let module_cfi_status = match module_id {
                                    Some(id) => {
                                        unwind_statuses.get(&id).copied().unwrap_or_default()
                                    }
                                    // TODO: Emit a custom status for code modules without debug_id
                                    None => ObjectFileStatus::Missing,
                                };

                                metric!(
                                    counter("symbolication.unwind_status") += 1,
                                    "status" => module_cfi_status.name()
                                );
                                info.unwind_status = Some(module_cfi_status);

                                let features = match module_id {
                                    Some(id) => {
                                        object_features.get(&id).copied().unwrap_or_default()
                                    }
                                    None => Default::default(),
                                };

                                info.features.merge(features);

                                info
                            })
                            .collect::<CodeModulesBuilder>();

                        // Finally iterate through the threads and build the stacktraces to
                        // return, marking used modules when they are referenced by a frame.
                        let requesting_thread_index: Option<usize> =
                            process_state.requesting_thread().try_into().ok();
                        let threads = process_state.threads();
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
            };

            Ok((request, minidump_state))
        };

        let result = self
            .threadpool
            .spawn_handle(lazy.bind_hub(sentry::Hub::current()))
            .await
            .map_err(|_| SymbolicationError::Canceled)?;

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
    ) -> Result<(SymbolicateStacktraces, MinidumpState), SymbolicationError> {
        let future = async move {
            let sources = Arc::new(sources);

            let referenced_modules = self
                .get_referenced_modules_from_minidump(minidump.clone())
                .await?;

            let cfi_caches = self
                .load_cfi_caches(scope.clone(), referenced_modules, sources.clone())
                .await?;

            self.stackwalk_minidump_with_cfi(scope, minidump, sources, cfi_caches)
                .await
        }
        .boxed_local()
        .compat();

        future_metrics!(
            "minidump_stackwalk",
            Some((Duration::from_secs(1200), SymbolicationError::Timeout)),
            future,
        )
        .compat()
        .await
    }

    async fn do_process_minidump(
        self,
        scope: Scope,
        minidump: Bytes,
        sources: Vec<SourceConfig>,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let (request, state) = self
            .clone()
            .do_stackwalk_minidump(scope, minidump, sources)
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
    ) -> RequestId {
        self.create_symbolication_request(
            self.clone().do_process_minidump(scope, minidump, sources),
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
                sources: Arc::new(sources),
                signal: None,
                stacktraces,
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

        let request_future = self
            .threadpool
            .spawn_handle(parse_future.bind_hub(sentry::Hub::current()))
            .boxed_local()
            .compat()
            .map_err(|_| SymbolicationError::Canceled)
            .flatten();

        future_metrics!(
            "parse_apple_crash_report",
            Some((Duration::from_secs(1200), SymbolicationError::Timeout)),
            request_future,
        )
        .compat()
        .await
    }

    async fn do_process_apple_crash_report(
        self,
        scope: Scope,
        report: Bytes,
        sources: Vec<SourceConfig>,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let (request, state) = self
            .parse_apple_crash_report(scope, report, sources)
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
    ) -> RequestId {
        self.create_symbolication_request(self.clone().do_process_apple_crash_report(
            scope,
            apple_crash_report,
            sources,
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
    /// directory. The directory is cleaned up when the `TempDir` instance is dropped. Keep it as
    /// guard until the test has finished.
    ///
    /// The service is configured with `connect_to_reserved_ips = True`. This allows to use a local
    /// symbol server to test object file downloads.
    fn setup_service() -> (ServiceState, test::TempDir) {
        test::setup();

        let cache_dir = test::tempdir();

        let mut config = Config::default();
        config.cache_dir = Some(cache_dir.path().to_owned());
        config.connect_to_reserved_ips = true;
        let service = ServiceState::create(config).unwrap();

        (service, cache_dir)
    }

    fn get_symbolication_request(sources: Vec<SourceConfig>) -> SymbolicateStacktraces {
        SymbolicateStacktraces {
            scope: Scope::Global,
            signal: None,
            sources: Arc::new(sources),
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
        }
    }

    #[test]
    fn test_remove_bucket() -> Result<(), SymbolicationError> {
        // Test with sources first, and then without. This test should verify that we do not leak
        // cached debug files to requests that no longer specify a source.

        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let response = test::block_fn01(|| {
            let request = get_symbolication_request(vec![source]);
            let request_id = service.symbolication().symbolicate_stacktraces(request);
            service.symbolication().get_response(request_id, None)
        })?;

        insta::assert_yaml_snapshot!(response);

        let response = test::block_fn01(|| {
            let request = get_symbolication_request(vec![]);
            let request_id = service.symbolication().symbolicate_stacktraces(request);
            service.symbolication().get_response(request_id, None)
        })?;

        insta::assert_yaml_snapshot!(response);

        Ok(())
    }

    #[test]
    fn test_add_bucket() -> Result<(), SymbolicationError> {
        // Test without sources first, then with. This test should verify that we apply a new source
        // to requests immediately.

        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let response = test::block_fn01(|| {
            let request = get_symbolication_request(vec![]);
            let request_id = service.symbolication().symbolicate_stacktraces(request);
            service.symbolication().get_response(request_id, None)
        })?;

        insta::assert_yaml_snapshot!(response);

        let response = test::block_fn01(|| {
            let request = get_symbolication_request(vec![source]);
            let request_id = service.symbolication().symbolicate_stacktraces(request);
            service.symbolication().get_response(request_id, None)
        })?;

        insta::assert_yaml_snapshot!(response);

        Ok(())
    }

    fn stackwalk_minidump(path: &str) -> Result<(), SymbolicationError> {
        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let minidump = Bytes::from(fs::read(path)?);
        let response = test::block_fn01(|| {
            let symbolication = service.symbolication();
            let request_id = symbolication.process_minidump(Scope::Global, minidump, vec![source]);
            service.symbolication().get_response(request_id, None)
        })?;

        insta::assert_yaml_snapshot!(response);

        let global_dir = service.config().cache_dir("object_meta/global").unwrap();
        let mut cache_entries: Vec<_> = fs::read_dir(global_dir)?
            .map(|x| x.unwrap().file_name().into_string().unwrap())
            .collect();

        cache_entries.sort();
        insta::assert_yaml_snapshot!(cache_entries);

        Ok(())
    }

    #[test]
    fn test_minidump_windows() -> Result<(), SymbolicationError> {
        stackwalk_minidump("./tests/fixtures/windows.dmp")
    }

    #[test]
    fn test_minidump_macos() -> Result<(), SymbolicationError> {
        stackwalk_minidump("./tests/fixtures/macos.dmp")
    }

    #[test]
    fn test_minidump_linux() -> Result<(), SymbolicationError> {
        stackwalk_minidump("./tests/fixtures/linux.dmp")
    }

    #[test]
    fn test_apple_crash_report() -> Result<(), SymbolicationError> {
        let (service, _cache_dir) = setup_service();
        let (_symsrv, source) = test::symbol_server();

        let report_file = Bytes::from(fs::read("./tests/fixtures/apple_crash_report.txt")?);
        let response = test::block_fn01(|| {
            let request_id = service.symbolication().process_apple_crash_report(
                Scope::Global,
                report_file,
                vec![source],
            );

            service.symbolication().get_response(request_id, None)
        })?;

        insta::assert_yaml_snapshot!(response);
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

        let (a, b, c) = lookup.lookup_symcache(43).unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, &info);
        assert!(c.is_none());
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
