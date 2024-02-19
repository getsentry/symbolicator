//! Provides access to the metrics sytem.
use std::collections::BTreeMap;
use std::fmt::Write;
use std::net::ToSocketAddrs;
use std::ops::Deref;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use std::{fmt, thread};

use cadence::{BufferedUdpMetricSink, MetricSink, QueuingMetricSink, StatsdClient};
use crossbeam_utils::CachePadded;
use rustc_hash::FxHashMap;
use thread_local::ThreadLocal;

static METRICS_CLIENT: OnceLock<MetricsWrapper> = OnceLock::new();

/// The metrics prelude that is necessary to use the client.
pub mod prelude {
    pub use cadence::prelude::*;
}

type LocalAggregators = Arc<ThreadLocal<CachePadded<Mutex<LocalAggregator>>>>;

#[derive(Debug, Clone)]
struct Sink(Arc<QueuingMetricSink>);

impl MetricSink for Sink {
    fn emit(&self, metric: &str) -> std::io::Result<usize> {
        self.0.emit(metric)
    }
    fn flush(&self) -> std::io::Result<()> {
        self.0.flush()
    }
}

pub struct MetricsWrapper {
    /// The raw `cadence` client.
    statsd_client: StatsdClient,

    /// A thread local aggregator for `count` metrics
    local_aggregator: LocalAggregators,
}

impl fmt::Debug for MetricsWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricsWrapper")
            .field("statsd_client", &self.statsd_client)
            .field(
                "local_aggregator",
                &format_args!("LocalAggregator {{ .. }}"),
            )
            .finish()
    }
}

fn make_aggregator(prefix: &str, formatted_global_tags: String, sink: Sink) -> LocalAggregators {
    let local_aggregator: LocalAggregators = Default::default();

    let aggregators = Arc::clone(&local_aggregator);
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{}.", prefix.trim_end_matches('.'))
    };

    let thread_fn = move || {
        let mut formatted_metric = String::with_capacity(256);
        loop {
            thread::sleep(Duration::from_secs(5));

            let mut total_aggregations = Aggregations::default();

            for local_aggregator in aggregators.iter() {
                let local_aggregations = {
                    let mut local_aggregator = local_aggregator.lock().unwrap();
                    std::mem::take(&mut local_aggregator.aggregations)
                };

                if total_aggregations.is_empty() {
                    total_aggregations = local_aggregations;
                } else {
                    for (key, value) in local_aggregations {
                        let aggregated_value = total_aggregations.entry(key).or_default();
                        *aggregated_value += value
                    }
                }
            }

            for (AggregationKey { name, tags }, value) in total_aggregations.drain() {
                let _ = write!(
                    &mut formatted_metric,
                    "{}{name}:{value}|c{}",
                    prefix, formatted_global_tags
                );

                if !tags.is_empty() {
                    if formatted_global_tags.is_empty() {
                        formatted_metric.push_str("|#");
                    } else {
                        formatted_metric.push(',');
                    }
                    formatted_metric.push_str(&tags);
                }

                let _ = sink.emit(&formatted_metric);

                formatted_metric.clear();
            }
        }
    };

    thread::Builder::new()
        .name("metrics-aggregator".into())
        .spawn(thread_fn)
        .unwrap();

    local_aggregator
}

impl MetricsWrapper {
    pub fn with_local_aggregator(&self, f: impl FnOnce(&mut LocalAggregator)) {
        let mut local_aggregator = self
            .local_aggregator
            .get_or(Default::default)
            .lock()
            .unwrap();
        f(&mut local_aggregator)
    }
}

impl Deref for MetricsWrapper {
    type Target = StatsdClient;

    fn deref(&self) -> &Self::Target {
        &self.statsd_client
    }
}

#[derive(Eq, Ord, PartialEq, PartialOrd, Hash)]
struct AggregationKey {
    name: Box<str>,
    tags: Box<str>,
}

type Aggregations = FxHashMap<AggregationKey, i64>;

#[derive(Default)]
pub struct LocalAggregator {
    buf: String,
    aggregations: Aggregations,
}

impl LocalAggregator {
    pub fn emit_count(&mut self, name: &str, value: i64, tags: &[(&str, &str)]) {
        self.buf.reserve(256);
        for (key, value) in tags {
            if !self.buf.is_empty() {
                self.buf.push(',');
            }
            let _ = write!(&mut self.buf, "{key}:{value}");
        }

        let key = AggregationKey {
            name: name.into(),
            tags: self.buf.as_str().into(),
        };
        self.buf.clear();

        let aggregation = self.aggregations.entry(key).or_default();
        *aggregation += value;
    }
}

/// Tell the metrics system to report to statsd.
pub fn configure_statsd<A: ToSocketAddrs>(prefix: &str, host: A, tags: BTreeMap<String, String>) {
    let addrs: Vec<_> = host.to_socket_addrs().unwrap().collect();
    if !addrs.is_empty() {
        tracing::info!("Reporting metrics to statsd at {}", addrs[0]);
    }
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let udp_sink = BufferedUdpMetricSink::from(&addrs[..], socket).unwrap();
    let queuing_sink = QueuingMetricSink::from(udp_sink);
    let sink = Sink(Arc::new(queuing_sink));

    let mut builder = StatsdClient::builder(prefix, sink.clone());

    let mut formatted_global_tags = String::new();
    for (key, value) in tags {
        if formatted_global_tags.is_empty() {
            formatted_global_tags.push_str("|#");
        } else {
            formatted_global_tags.push(',');
        }
        let _ = write!(&mut formatted_global_tags, "{key}:{value}");

        builder = builder.with_tag(key, value)
    }
    let statsd_client = builder.build();

    let local_aggregator = make_aggregator(prefix, formatted_global_tags, sink);

    let wrapper = MetricsWrapper {
        statsd_client,
        local_aggregator,
    };

    METRICS_CLIENT.set(wrapper).unwrap();
}

/// Invoke a callback with the current [`StatsdClient`].
///
/// If no [`StatsdClient`] is configured the callback is not invoked.
/// For the most part the [`metric!`](crate::metric) macro should be used instead.
#[inline(always)]
pub fn with_client<F>(f: F)
where
    F: FnOnce(&MetricsWrapper),
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
        $crate::metrics::with_client(|client| {
            client.with_local_aggregator(|local| {
                let tags: &[(&str, &str)] = &[
                    $(($k, $v)),*
                ];
                local.emit_count($id, $value, tags);
            });
        });
    }};

    // gauges
    (gauge($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|client| {
            use $crate::metrics::prelude::*;
            client
                .gauge_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // timers
    (timer($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|client| {
            use $crate::metrics::prelude::*;
            client
                .time_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // we use statsd timers to send things such as filesizes as well.
    (time_raw($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|client| {
            use $crate::metrics::prelude::*;
            client
                .time_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // histograms
    (histogram($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|client| {
            use $crate::metrics::prelude::*;
            client
                .histogram_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};
}
