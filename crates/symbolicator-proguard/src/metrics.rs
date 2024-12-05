use std::collections::HashMap;

use symbolicator_service::{metric, types::Platform};

/// Record metrics about exceptions, stacktraces, frames, and remapped classes.
pub(crate) fn record_symbolication_metrics(
    event_platform: Option<Platform>,
    stats: SymbolicationStats,
) {
    let event_platform = event_platform
        .as_ref()
        .map(|p| p.as_ref())
        .unwrap_or("none");

    metric!(time_raw("symbolication.num_exceptions") = stats.symbolicated_exceptions, "event_platform" => event_platform);
    metric!(time_raw("symbolication.unsymbolicated_exceptions") = stats.unsymbolicated_exceptions, "event_platform" => event_platform);

    metric!(time_raw("symbolication.num_stacktraces") = stats.num_stacktraces);

    for (p, count) in stats.symbolicated_frames {
        let frame_platform = p.as_ref().map(|p| p.as_ref()).unwrap_or("none");
        metric!(
            time_raw("symbolication.num_frames") =
                count,
            "frame_platform" => frame_platform, "event_platform" => event_platform
        );
    }

    for (p, count) in stats.unsymbolicated_frames {
        let frame_platform = p.as_ref().map(|p| p.as_ref()).unwrap_or("none");
        metric!(
            time_raw("symbolication.unsymbolicated_frames") =
                count,
            "frame_platform" => frame_platform, "event_platform" => event_platform
        );
    }

    metric!(time_raw("symbolication.num_classes") = stats.symbolicated_classes, "event_platform" => event_platform);
    metric!(time_raw("symbolication.unsymbolicated_classes") = stats.unsymbolicated_classes, "event_platform" => event_platform);
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolicationStats {
    pub(crate) symbolicated_exceptions: u64,
    pub(crate) unsymbolicated_exceptions: u64,
    pub(crate) symbolicated_classes: u64,
    pub(crate) unsymbolicated_classes: u64,
    pub(crate) symbolicated_frames: HashMap<Option<Platform>, u64>,
    pub(crate) unsymbolicated_frames: HashMap<Option<Platform>, u64>,
    pub(crate) num_stacktraces: u64,
}
