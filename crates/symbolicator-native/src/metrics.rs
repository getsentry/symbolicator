use std::collections::HashMap;

use symbolic::common::DebugId;
use symbolicator_service::types::ObjectFileStatus;
use symbolicator_service::{metric, types::Platform};
use symbolicator_sources::ObjectType;

use crate::interface::{CompleteObjectInfo, CompleteStacktrace};

#[derive(Debug, Copy, Clone)]
/// Where the Stack Traces in the [`SymbolicateStacktraces`](crate::interface::SymbolicateStacktraces)
/// originated from.
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

/// Stacktrace related Metrics
///
/// This gives some metrics about the quality of the stack traces included
/// in a symbolication request. See the individual members for more information.
///
/// These numbers are being accumulated across one symbolication request, and are emitted
/// as a histogram.
#[derive(Default)]
pub struct StacktraceMetrics {
    /// A truncated stack trace is one that does not end in a
    /// well known thread base.
    pub truncated_traces: u64,

    /// We classify a short stacktrace as one that has less that 5 frames.
    pub short_traces: u64,

    /// This indicated a stack trace that has at least one bad frame
    /// from the below categories.
    pub bad_traces: u64,

    /// Frames that were scanned.
    ///
    /// These are frequently wrong and lead to bad and incomplete stack traces.
    /// We can improve (lower) these numbers by having more usable CFI info.
    pub scanned_frames: u64,

    /// Unsymbolicated Frames.
    ///
    /// These may be the result of unavailable or broken debug info.
    /// We can improve (lower) these numbers by having more usable debug info.
    pub unsymbolicated_frames: u64,

    /// Unsymbolicated Context Frames.
    ///
    /// This is an indication of broken contexts, or failure to extract it from minidumps.
    pub unsymbolicated_context_frames: u64,

    /// Unsymbolicated Frames found by scanning.
    pub unsymbolicated_scanned_frames: u64,

    /// Unsymbolicated Frames found by CFI.
    ///
    /// These are the result of the *previous* frame being wrongly scanned.
    pub unsymbolicated_cfi_frames: u64,

    /// Frames referencing unmapped memory regions.
    ///
    /// These may be the result of issues in the client-side module finder, or
    /// broken debug-id information.
    ///
    /// We can improve this by fixing client-side implementations and having
    /// proper debug-ids.
    pub unmapped_frames: u64,
}

pub fn record_symbolication_metrics(
    event_platform: Option<Platform>,
    origin: StacktraceOrigin,
    metrics: StacktraceMetrics,
    modules: &[CompleteObjectInfo],
    stacktraces: &[CompleteStacktrace],
) {
    let origin = origin.to_string();

    let event_platform = event_platform
        .as_ref()
        .map(|p| p.as_ref())
        .unwrap_or("none");

    let object_platform = modules
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
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unusable_modules") = unusable_modules,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unparsable_modules") = unparsable_modules,
        "platform" => &object_platform, "origin" => &origin
    );

    metric!(
        time_raw("symbolication.num_stacktraces") = stacktraces.len() as u64,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.short_stacktraces") = metrics.short_traces,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.truncated_stacktraces") = metrics.truncated_traces,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.bad_stacktraces") = metrics.bad_traces,
        "platform" => &object_platform, "origin" => &origin
    );

    // Count number of frames by platform (including no platform)
    let frames_by_platform = stacktraces.iter().flat_map(|st| st.frames.iter()).fold(
        HashMap::new(),
        |mut map, frame| {
            let platform = frame.raw.platform.as_ref();
            let count: &mut usize = map.entry(platform).or_default();
            *count += 1;
            map
        },
    );

    for (p, count) in &frames_by_platform {
        let frame_platform = p.map(|p| p.as_ref()).unwrap_or("none");
        metric!(
            time_raw("symbolication.num_frames") =
                count,
            "platform" => &object_platform, "origin" => &origin,
            "frame_platform" => frame_platform, "event_platform" => event_platform
        );
    }
    metric!(
        time_raw("symbolication.scanned_frames") = metrics.scanned_frames,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unsymbolicated_frames") = metrics.unsymbolicated_frames,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unsymbolicated_context_frames") =
            metrics.unsymbolicated_context_frames,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unsymbolicated_cfi_frames") =
            metrics.unsymbolicated_cfi_frames,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unsymbolicated_scanned_frames") =
            metrics.unsymbolicated_scanned_frames,
        "platform" => &object_platform, "origin" => &origin
    );
    metric!(
        time_raw("symbolication.unmapped_frames") = metrics.unmapped_frames,
        "platform" => &object_platform, "origin" => &origin
    );
}
