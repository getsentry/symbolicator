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

    let event_platform = event_platform.to_owned();

    metric!(distribution("symbolication.num_exceptions") = stats.symbolicated_exceptions as f64, "event_platform" => event_platform.clone());
    metric!(distribution("symbolication.unsymbolicated_exceptions") = stats.unsymbolicated_exceptions as f64, "event_platform" => event_platform.clone());

    metric!(distribution("symbolication.num_stacktraces") = stats.num_stacktraces as f64);

    for (p, count) in stats.symbolicated_frames {
        let frame_platform = p.as_ref().map(|p| p.as_ref()).unwrap_or("none").to_owned();
        metric!(
            distribution("symbolication.num_frames") =
                count as f64,
            "frame_platform" => frame_platform, "event_platform" => event_platform.clone()
        );
    }

    for (p, count) in stats.unsymbolicated_frames {
        let frame_platform = p.as_ref().map(|p| p.as_ref()).unwrap_or("none").to_owned();
        metric!(
            distribution("symbolication.unsymbolicated_frames") =
                count as f64,
            "frame_platform" => frame_platform, "event_platform" => event_platform.clone()
        );
    }

    metric!(distribution("symbolication.num_classes") = stats.symbolicated_classes as f64, "event_platform" => event_platform.clone());
    metric!(distribution("symbolication.unsymbolicated_classes") = stats.unsymbolicated_classes as f64, "event_platform" => event_platform);
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
