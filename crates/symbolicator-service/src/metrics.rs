//! Provides access to the metrics system.
use std::collections::BTreeMap;

use metrics_exporter_dogstatsd::{AggregationMode, DogStatsDBuilder};

#[doc(hidden)]
pub mod _metrics {
    pub use metrics::*;
}

/// Tell the metrics system to report to statsd.
pub fn configure_statsd(
    prefix: &str,
    addr: &str,
    tags: BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let default_labels = tags
        .into_iter()
        .map(|(key, value)| metrics::Label::new(key, value))
        .collect();

    DogStatsDBuilder::default()
        .with_remote_address(addr)?
        .with_telemetry(true)
        .with_aggregation_mode(AggregationMode::Aggressive)
        .send_histograms_as_distributions(true)
        .with_histogram_sampling(false)
        .set_global_prefix(prefix)
        .with_global_labels(default_labels)
        .install()?;

    Ok(())
}

/// Emits a metric.
#[macro_export]
macro_rules! metric {
    // counters
    (counter($($id:tt)+) += $value:expr $(, $($tt:tt)*)?) => {{
        $crate::metrics::_metrics::counter!($($id)* $(, $($tt)*)?).increment($value)
    }};

    // gauges
    (gauge($($id:tt)+) = $value:expr $(, $($tt:tt)*)?) => {{
        $crate::metrics::_metrics::gauge!($($id)* $(, $($tt)*)?).set($value)
    }};

    // timers
    (timer($($id:tt)+) = $value:expr $(, $($tt:tt)*)?) => {{ $crate::metrics::_metrics::histogram!($($id)* $(, $($tt)*)?).record($value.as_nanos() as f64 / 1e6) }};

    // distributions
    (distribution($($id:tt)+) = $value:expr $(, $($tt:tt)*)?) => {{ $crate::metrics::_metrics::histogram!($($id)* $(, $($tt)*)?).record($value) }};
}
