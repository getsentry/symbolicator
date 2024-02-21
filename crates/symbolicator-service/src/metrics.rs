//! Provides access to the metrics sytem.
use std::collections::BTreeMap;
use std::net::ToSocketAddrs;
use std::sync::OnceLock;

use cadence::StatsdClient;
use statsdproxy::cadence::StatsdProxyMetricSink;
use statsdproxy::config::AggregateMetricsConfig;
use statsdproxy::middleware::aggregate::AggregateMetrics;
use statsdproxy::middleware::upstream::Upstream;

static METRICS_CLIENT: OnceLock<StatsdClient> = OnceLock::new();

/// The metrics prelude that is necessary to use the client.
pub mod prelude {
    pub use cadence::prelude::*;
}

/// Tell the metrics system to report to statsd.
pub fn configure_statsd<A: ToSocketAddrs>(prefix: &str, host: A, tags: BTreeMap<String, String>) {
    let addrs: Vec<_> = host.to_socket_addrs().unwrap().collect();
    if !addrs.is_empty() {
        tracing::info!("Reporting metrics to statsd at {}", addrs[0]);
    }
    let aggregator_sink = StatsdProxyMetricSink::new(move || {
        let next_step = Upstream::new(&addrs[..]).unwrap();

        let config = AggregateMetricsConfig {
            aggregate_counters: true,
            aggregate_gauges: true,
            flush_offset: 0,
            flush_interval: 5,
            max_map_size: None,
        };
        AggregateMetrics::new(config, next_step)
    });

    let mut builder = StatsdClient::builder(prefix, aggregator_sink);
    for (key, value) in tags {
        builder = builder.with_tag(key, value)
    }
    let client = builder.build();

    METRICS_CLIENT.set(client).unwrap();
}

/// Invoke a callback with the current [`StatsdClient`].
///
/// If no [`StatsdClient`] is configured the callback is not invoked.
/// For the most part the [`metric!`](crate::metric) macro should be used instead.
#[inline(always)]
pub fn with_client<F>(f: F)
where
    F: FnOnce(&StatsdClient),
{
    if let Some(client) = METRICS_CLIENT.get() {
        f(client)
    }
}

/// Emits a metric.
#[macro_export]
macro_rules! metric {
    // counters
    (counter($id:expr) += $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client
                .count_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};
    (counter($id:expr) -= $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client
                .count_with_tags($id, -$value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // gauges
    (gauge($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client
                .gauge_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // timers
    (timer($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client
                .time_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // we use statsd timers to send things such as filesizes as well.
    (time_raw($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client
                .time_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // histograms
    (histogram($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client
                .histogram_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};
}
