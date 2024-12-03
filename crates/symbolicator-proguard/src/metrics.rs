use std::{collections::HashMap, sync::Arc};

use symbolicator_service::{metric, types::Platform};

use crate::interface::{JvmException, JvmStacktrace};

/// Record metrics about exceptions, stacktraces, frames, and remapped classes.
pub fn record_symbolication_metrics(
    event_platform: Option<Platform>,
    exceptions: &[JvmException],
    stacktraces: &[JvmStacktrace],
    classes: &HashMap<Arc<str>, Arc<str>>,
    unsymbolicated_frames: u64,
) {
    let event_platform = event_platform
        .as_ref()
        .map(|p| p.as_ref())
        .unwrap_or("none");

    metric!(time_raw("symbolication.num_exceptions") = exceptions.len() as u64, "event_platform" => event_platform);
    metric!(time_raw("symbolication.num_stacktraces") = stacktraces.len() as u64);

    // Count number of frames by platform (including no platform)
    let frames_by_platform = stacktraces.iter().flat_map(|st| st.frames.iter()).fold(
        HashMap::new(),
        |mut map, frame| {
            let platform = frame.platform.as_ref();
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
            "frame_platform" => frame_platform, "event_platform" => event_platform
        );
    }
    metric!(time_raw("symbolication.num_classes") = classes.len() as u64, "event_platform" => event_platform);
    metric!(time_raw("symbolication.unsymbolicated_frames") = unsymbolicated_frames);
}
