use std::{collections::HashMap, sync::Arc};

use symbolicator_service::metric;

use crate::interface::{JvmException, JvmStacktrace};

/// Record metrics about exceptions, stacktraces, frames, and remapped classes.
pub fn record_symbolication_metrics(
    exceptions: &[JvmException],
    stacktraces: &[JvmStacktrace],
    classes: &HashMap<Arc<str>, Arc<str>>,
    unsymbolicated_frames: u64,
) {
    metric!(time_raw("symbolication.num_exceptions") = exceptions.len() as u64);
    metric!(time_raw("symbolication.num_stacktraces") = stacktraces.len() as u64);
    metric!(
        time_raw("symbolication.num_frames") = stacktraces
            .iter()
            .map(|s| s.frames.len() as u64)
            .sum::<u64>()
    );
    metric!(time_raw("symbolication.num_classes") = classes.len() as u64);
    metric!(time_raw("symbolication.unsymbolicated_frames") = unsymbolicated_frames);
}
