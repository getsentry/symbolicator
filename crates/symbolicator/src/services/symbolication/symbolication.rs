use std::sync::Arc;
use std::time::Duration;

use symbolic::common::{split_path, DebugId, InstructionInfo, Language, Name};
use symbolic::demangle::{Demangle, DemangleOptions};

use crate::sources::SourceConfig;
use crate::types::{
    CompleteObjectInfo, CompleteStacktrace, CompletedSymbolicationResponse, FrameStatus,
    FrameTrust, ObjectFileStatus, ObjectType, RawFrame, RawStacktrace, Registers, RequestOptions,
    Scope, Signal, SymbolicatedFrame,
};
use crate::utils::futures::{m, measure};
use crate::utils::hex::HexValue;

use super::module_lookup::ModuleLookup;
use super::{SymbolicationActor, SymbolicationError};

impl SymbolicationActor {
    #[tracing::instrument(skip_all)]
    pub(super) async fn do_symbolicate(
        &self,
        request: SymbolicateStacktraces,
    ) -> Result<CompletedSymbolicationResponse, SymbolicationError> {
        let serialize_dif_candidates = request.options.dif_candidates;

        let f = self.do_symbolicate_impl(request);
        let f = tokio::time::timeout(Duration::from_secs(3600), f);
        let f = measure("symbolicate", m::timed_result, None, f);

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
        &self,
        request: SymbolicateStacktraces,
    ) -> Result<CompletedSymbolicationResponse, anyhow::Error> {
        let SymbolicateStacktraces {
            stacktraces,
            sources,
            scope,
            signal,
            origin,
            modules,
            ..
        } = request;

        let mut module_lookup = ModuleLookup::new(scope, sources, modules.into_iter());
        module_lookup
            .fetch_symcaches(self.symcaches.clone(), &stacktraces)
            .await;

        let mut metrics = StacktraceMetrics::default();
        let mut stacktraces: Vec<_> = stacktraces
            .into_iter()
            .map(|trace| symbolicate_stacktrace(trace, &module_lookup, &mut metrics, signal))
            .collect();

        module_lookup
            .fetch_sources(self.objects.clone(), &stacktraces)
            .await;

        let debug_sessions = module_lookup.prepare_debug_sessions();

        for trace in &mut stacktraces {
            for frame in &mut trace.frames {
                let (abs_path, lineno) = match (&frame.raw.abs_path, frame.raw.lineno) {
                    (&Some(ref abs_path), Some(lineno)) => (abs_path, lineno),
                    _ => continue,
                };

                let result = module_lookup.get_context_lines(
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
        // explicitly drop this, so it does not borrow `module_lookup` anymore.
        drop(debug_sessions);

        // bring modules back into the original order
        let modules = module_lookup.into_inner();
        record_symbolication_metrics(origin, metrics, &modules, &stacktraces);

        Ok(CompletedSymbolicationResponse {
            signal,
            stacktraces,
            modules,
            ..Default::default()
        })
    }
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

    /// Where the stacktraces originated from.
    pub origin: StacktraceOrigin,

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

fn symbolicate_frame(
    caches: &ModuleLookup,
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

    tracing::trace!("Loading symcache");
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
                    tracing::warn!(
                            "Underflow when trying to subtract image start addr from caller address after heuristics"
                        );
                    metric!(counter("relative_addr.underflow") += 1);
                    FrameStatus::MissingSymbol
                })?
        } else {
            addr
        }
    } else {
        tracing::warn!("Underflow when trying to subtract image start addr from caller address before heuristics");
        metric!(counter("relative_addr.underflow") += 1);
        return Err(FrameStatus::MissingSymbol);
    };

    tracing::trace!("Symbolicating {:#x}", relative_addr);
    let mut rv = vec![];

    // The symbol addr only makes sense for the outermost top-level function, and not its inlinees.
    // We keep track of it while iterating and only set it for the last frame,
    // which is the top-level function.
    let mut sym_addr = None;
    let instruction_addr = HexValue(lookup_result.expose_preferred_addr(relative_addr));

    for source_location in symcache.lookup(relative_addr) {
        let abs_path = source_location
            .file()
            .map(|f| f.full_path())
            .unwrap_or_default();
        let filename = split_path(&abs_path).1;

        let func = source_location.function();
        let symbol = func.name();

        // Detect the language from the bare name, ignoring any pre-set language. There are a few
        // languages that we should always be able to demangle. Only complain about those that we
        // detect explicitly, but silently ignore the rest. For instance, there are C-identifiers
        // reported as C++, which are expected not to demangle.
        let detected_language = Name::from(symbol).detect_language();
        let should_demangle = match (func.language(), detected_language) {
            (_, Language::Unknown) => false, // can't demangle what we cannot detect
            (Language::ObjCpp, Language::Cpp) => true, // C++ demangles even if it was in ObjC++
            (Language::Unknown, _) => true,  // if there was no language, then rely on detection
            (lang, detected) => lang == detected, // avoid false-positive detections
        };

        let demangled_opt = func.name_for_demangling().demangle(DEMANGLE_OPTIONS);
        if should_demangle && demangled_opt.is_none() {
            sentry::with_scope(
                |scope| scope.set_extra("identifier", symbol.to_string().into()),
                || {
                    let message = format!("Failed to demangle {} identifier", func.language());
                    sentry::capture_message(&message, sentry::Level::Error);
                },
            );
        }

        sym_addr = Some(HexValue(
            lookup_result.expose_preferred_addr(func.entry_pc() as u64),
        ));
        let filename = if !filename.is_empty() {
            Some(filename.to_string())
        } else {
            frame.filename.clone()
        };
        rv.push(SymbolicatedFrame {
            status: FrameStatus::Symbolicated,
            original_index: Some(index),
            raw: RawFrame {
                package: lookup_result.object_info.raw.code_file.clone(),
                addr_mode: lookup_result.preferred_addr_mode(),
                instruction_addr,
                symbol: Some(symbol.to_string()),
                abs_path: if !abs_path.is_empty() {
                    Some(abs_path)
                } else {
                    frame.abs_path.clone()
                },
                function: Some(match demangled_opt {
                    Some(demangled) => demangled,
                    None => symbol.to_string(),
                }),
                filename,
                lineno: Some(source_location.line()),
                pre_context: vec![],
                context_line: None,
                post_context: vec![],
                sym_addr: None,
                lang: match func.language() {
                    Language::Unknown => None,
                    language => Some(language),
                },
                trust: frame.trust,
            },
        });
    }

    if let Some(last_frame) = rv.last_mut() {
        last_frame.raw.sym_addr = sym_addr;
    }

    if rv.is_empty() {
        return Err(FrameStatus::MissingSymbol);
    }

    Ok(rv)
}

/// Options for demangling all symbols.
const DEMANGLE_OPTIONS: DemangleOptions = DemangleOptions::complete().return_type(false);

/// Stacktrace related Metrics
///
/// This gives some metrics about the quality of the stack traces included
/// in a symbolication request. See the individual members for more information.
///
/// These numbers are being accumulated across one symbolication request, and are emitted
/// as a histogram.
#[derive(Default)]
struct StacktraceMetrics {
    /// A truncated stack trace is one that does not end in a
    /// well known thread base.
    truncated_traces: u64,

    /// We classify a short stacktrace as one that has less that 5 frames.
    short_traces: u64,

    /// This indicated a stack trace that has at least one bad frame
    /// from the below categories.
    bad_traces: u64,

    /// Frames that were scanned.
    ///
    /// These are frequently wrong and lead to bad and incomplete stack traces.
    /// We can improve (lower) these numbers by having more usable CFI info.
    scanned_frames: u64,

    /// Unsymbolicated Frames.
    ///
    /// These may be the result of unavailable or broken debug info.
    /// We can improve (lower) these numbers by having more usable debug info.
    unsymbolicated_frames: u64,

    /// Unsymbolicated Context Frames.
    ///
    /// This is an indication of broken contexts, or failure to extract it from minidumps.
    unsymbolicated_context_frames: u64,

    /// Unsymbolicated Frames found by scanning.
    unsymbolicated_scanned_frames: u64,

    /// Unsymbolicated Frames found by CFI.
    ///
    /// These are the result of the *previous* frame being wrongly scanned.
    unsymbolicated_cfi_frames: u64,

    /// Frames referencing unmapped memory regions.
    ///
    /// These may be the result of issues in the client-side module finder, or
    /// broken debug-id information.
    ///
    /// We can improve this by fixing client-side implementations and having
    /// proper debug-ids.
    unmapped_frames: u64,
}

/// Determine if the [`SymbolicatedFrame`] is likely to be a thread base.
///
/// This is just a heuristic that matches the function to well known thread entry points.
fn is_likely_base_frame(frame: &SymbolicatedFrame) -> bool {
    let function = match frame
        .raw
        .function
        .as_deref()
        .or(frame.raw.symbol.as_deref())
    {
        Some(f) => f,
        None => return false,
    };

    // C start/main
    if matches!(function, "main" | "start" | "_start") {
        return true;
    }

    // Windows and posix thread base. These often have prefixes depending on the OS and Version, so
    // we use a substring match here.
    if function.contains("UserThreadStart")
        || function.contains("thread_start")
        || function.contains("start_thread")
        || function.contains("start_wqthread")
    {
        return true;
    }

    false
}

fn record_symbolication_metrics(
    origin: StacktraceOrigin,
    metrics: StacktraceMetrics,
    modules: &[CompleteObjectInfo],
    stacktraces: &[CompleteStacktrace],
) {
    let origin = origin.to_string();

    let platform = modules
        .iter()
        .find_map(|m| {
            if m.raw.ty == ObjectType::Unknown {
                None
            } else {
                Some(m.raw.ty)
            }
        })
        .unwrap_or(ObjectType::Unknown)
        .to_string();

    // Unusable modules that don’t have any kind of ID to look them up with
    let mut unusable_modules = 0;
    // Modules that failed parsing
    let mut unparsable_modules = 0;

    for m in modules {
        metric!(
            counter("symbolication.debug_status") += 1,
            "status" => m.debug_status.name()
        );

        let usable_code_id = !matches!(m.raw.code_id.as_deref(), None | Some(""));

        // NOTE: this is a closure as a way to short-circuit the computation because
        // it is expensive
        let usable_debug_id = || match m.raw.debug_id.as_deref() {
            None | Some("") => false,
            Some(string) => string.parse::<DebugId>().is_ok(),
        };

        if !usable_code_id && !usable_debug_id() {
            unusable_modules += 1;
        }

        if m.debug_status == ObjectFileStatus::Malformed {
            unparsable_modules += 1;
        }
    }

    metric!(
        time_raw("symbolication.num_modules") = modules.len() as u64,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unusable_modules") = unusable_modules,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unparsable_modules") = unparsable_modules,
        "platform" => &platform, "origin" => &origin
    );

    metric!(
        time_raw("symbolication.num_stacktraces") = stacktraces.len() as u64,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.short_stacktraces") = metrics.short_traces,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.truncated_stacktraces") = metrics.truncated_traces,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.bad_stacktraces") = metrics.bad_traces,
        "platform" => &platform, "origin" => &origin
    );

    metric!(
        time_raw("symbolication.num_frames") =
            stacktraces.iter().map(|s| s.frames.len() as u64).sum::<u64>(),
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.scanned_frames") = metrics.scanned_frames,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unsymbolicated_frames") = metrics.unsymbolicated_frames,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unsymbolicated_context_frames") =
            metrics.unsymbolicated_context_frames,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unsymbolicated_cfi_frames") =
            metrics.unsymbolicated_cfi_frames,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unsymbolicated_scanned_frames") =
            metrics.unsymbolicated_scanned_frames,
        "platform" => &platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unmapped_frames") = metrics.unmapped_frames,
        "platform" => &platform, "origin" => &origin
    );
}

fn symbolicate_stacktrace(
    thread: RawStacktrace,
    caches: &ModuleLookup,
    metrics: &mut StacktraceMetrics,
    signal: Option<Signal>,
) -> CompleteStacktrace {
    let mut symbolicated_frames = vec![];
    let mut unsymbolicated_frames_iter = thread.frames.into_iter().enumerate().peekable();

    while let Some((index, mut frame)) = unsymbolicated_frames_iter.next() {
        match symbolicate_frame(caches, &thread.registers, signal, &mut frame, index) {
            Ok(frames) => {
                if matches!(frame.trust, FrameTrust::Scan) {
                    metrics.scanned_frames += 1;
                }
                symbolicated_frames.extend(frames)
            }
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

                // Glibc inserts an explicit `DW_CFA_undefined: RIP` DWARF rule to say that `_start`
                // has no return address.
                // See https://sourceware.org/git/?p=glibc.git;a=blob;f=sysdeps/x86_64/start.S;h=1b3e36826b8a477474cee24d1c931429fbdf6d8f;hb=HEAD#l59
                // We do not support this due to lack of breakpad support, and will thus use the
                // previous rule for RIP, which says to look it up the value on the stack,
                // resulting in an unmapped garbage frame. We work around this by trimming the
                // trailing garbage frame on the following conditions:
                // * it is unmapped (UnknownImage)
                // * this is the last frame to symbolicate (via peek)
                // * the previous symbolicated frame is `_start`
                let is_start =
                    |frame: &SymbolicatedFrame| frame.raw.function.as_deref() == Some("_start");
                if status == FrameStatus::UnknownImage
                    && unsymbolicated_frames_iter.peek().is_none()
                    && symbolicated_frames.last().map_or(false, is_start)
                {
                    continue;
                }

                metrics.unsymbolicated_frames += 1;
                match frame.trust {
                    FrameTrust::Scan => {
                        metrics.scanned_frames += 1;
                        metrics.unsymbolicated_scanned_frames += 1;
                    }
                    FrameTrust::Cfi => metrics.unsymbolicated_cfi_frames += 1,
                    FrameTrust::Context => metrics.unsymbolicated_context_frames += 1,
                    _ => {}
                }
                if status == FrameStatus::UnknownImage {
                    metrics.unmapped_frames += 1;
                }

                symbolicated_frames.push(SymbolicatedFrame {
                    status,
                    original_index: Some(index),
                    raw: frame,
                });
            }
        }
    }

    // we try to find a base frame among the bottom 5
    if !symbolicated_frames
        .iter()
        .rev()
        .take(5)
        .any(is_likely_base_frame)
    {
        metrics.truncated_traces += 1;
    }
    // macOS has some extremely short but perfectly fine stacks, such as:
    // `__workq_kernreturn` > `_pthread_wqthread` > `start_wqthread`
    if symbolicated_frames.len() < 3 {
        metrics.short_traces += 1;
    }

    if metrics.scanned_frames > 0 || metrics.unsymbolicated_frames > 0 {
        metrics.bad_traces += 1;
    }

    CompleteStacktrace {
        thread_id: thread.thread_id,
        thread_name: thread.thread_name,
        is_requesting: thread.is_requesting,
        registers: thread.registers,
        frames: symbolicated_frames,
    }
}

#[derive(Debug, Copy, Clone)]
/// Where the Stack Traces in the [`SymbolicateStacktraces`] originated from.
pub enum StacktraceOrigin {
    /// The stack traces came from a direct request to symbolicate.
    Symbolicate,
    /// The stack traces were extracted from a minidump.
    Minidump,
    /// The stack traces came from an Apple Crash Report.
    AppleCrashReport,
}

impl std::fmt::Display for StacktraceOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            StacktraceOrigin::Symbolicate => "symbolicate",
            StacktraceOrigin::Minidump => "minidump",
            StacktraceOrigin::AppleCrashReport => "applecrashreport",
        })
    }
}
