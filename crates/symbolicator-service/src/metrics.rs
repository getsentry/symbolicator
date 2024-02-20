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

type LocalAggregators = Arc<ThreadLocal<CachePadded<Mutex<LocalAggregator>>>>;

/// The globally configured Metrics, including a `cadence` client, and a local aggregator.
pub struct MetricsWrapper {
    /// The raw `cadence` client.
    statsd_client: StatsdClient,

    /// A thread local aggregator.
    local_aggregator: LocalAggregators,
}

impl fmt::Debug for MetricsWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricsWrapper")
            .field("statsd_client", &self.statsd_client)
            .field(
                "local_aggregator",
                &format_args!("LocalAggregators {{ .. }}"),
            )
            .finish()
    }
}

/// Creates [`LocalAggregators`] and starts a thread that will periodically
/// send aggregated metrics upstream to the `sink`.
fn make_aggregator(prefix: &str, formatted_global_tags: String, sink: Sink) -> LocalAggregators {
    let local_aggregators: LocalAggregators = Default::default();

    let aggregators = Arc::clone(&local_aggregators);
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{}.", prefix.trim_end_matches('.'))
    };

    let thread_fn = move || {
        let mut formatted_metric = String::with_capacity(256);
        loop {
            thread::sleep(Duration::from_secs(5));

            let mut total_counters = AggregatedCounters::default();

            for local_counters in aggregators.iter() {
                let local_counters = {
                    let mut local_aggregator = local_counters.lock().unwrap();
                    std::mem::take(&mut local_aggregator.aggregated_counters)
                };

                if total_counters.is_empty() {
                    total_counters = local_counters;
                } else {
                    for (key, value) in local_counters {
                        let ty = key.ty;
                        let aggregated_value = total_counters.entry(key).or_default();
                        if ty == "|c" {
                            *aggregated_value += value;
                        } else if ty == "|g" {
                            // FIXME: when aggregating multiple thread-locals, we don’t really
                            // know which one is the "latest". But it also does not really matter.
                            *aggregated_value = value;
                        }
                    }
                }
            }

            for (AggregationKey { ty, name, tags }, value) in total_counters.drain() {
                let _ = write!(
                    &mut formatted_metric,
                    "{prefix}{name}:{value}{ty}{formatted_global_tags}"
                );

                if let Some(tags) = tags {
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

    local_aggregators
}

impl MetricsWrapper {
    /// Invokes the provided callback with a mutable reference to a thread-local [`LocalAggregator`].
    fn with_local_aggregator(&self, f: impl FnOnce(&Self, &mut LocalAggregator)) {
        let mut local_aggregator = self
            .local_aggregator
            .get_or(Default::default)
            .lock()
            .unwrap();
        f(self, &mut local_aggregator)
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
    /// The metric type, pre-formatted as a statsd suffix such as `|c`.
    ty: &'static str,
    /// The name of the metric.
    name: &'static str,
    /// The metric tags, pre-formatted as a statsd suffix, excluding the `|#` prefix.
    tags: Option<Box<str>>,
}

type AggregatedCounters = FxHashMap<AggregationKey, i64>;
type AggregatedHistograms = FxHashMap<AggregationKey, Vec<f64>>;

#[derive(Default)]
pub struct LocalAggregator {
    /// A mutable scratch-buffer that is reused to format tags into it.
    buf: String,
    /// A map of all the `counter` and `gauge` metrics we have aggregated thus far.
    aggregated_counters: AggregatedCounters,
    /// A map of all the `timer` and `histogram` metrics we have aggregated thus far.
    #[allow(unused)] // TODO
    aggregated_histograms: AggregatedHistograms,
}

impl LocalAggregator {
    /// Formats the `tags` into a `statsd` like format with the help of our scratch buffer.
    fn format_tags(&mut self, tags: &[(&str, &str)]) -> Option<Box<str>> {
        if tags.is_empty() {
            return None;
        }

        // to avoid reallocation, just reserve some space.
        // the size is rather arbitrary, but should be large enough for reasonable tags.
        self.buf.reserve(128);
        for (key, value) in tags {
            if !self.buf.is_empty() {
                self.buf.push(',');
            }
            let _ = write!(&mut self.buf, "{key}:{value}");
        }
        let formatted_tags = self.buf.as_str().into();
        self.buf.clear();

        Some(formatted_tags)
    }

    /// Emit a `count` metric, which is aggregated by summing up all values.
    pub fn emit_count(&mut self, name: &'static str, value: i64, tags: &[(&'static str, &str)]) {
        let tags = self.format_tags(tags);

        let key = AggregationKey {
            ty: "|c",
            name,
            tags,
        };

        let aggregation = self.aggregated_counters.entry(key).or_default();
        *aggregation += value;
    }

    /// Emit a `gauge` metric, for which only the latest value is retained.
    pub fn emit_gauge(&mut self, name: &'static str, value: u64, tags: &[(&'static str, &str)]) {
        let tags = self.format_tags(tags);

        let key = AggregationKey {
            // TODO: maybe we want to give gauges their own aggregations?
            ty: "|g",
            name,
            tags,
        };

        let aggregation = self.aggregated_counters.entry(key).or_default();
        *aggregation = value as i64;
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

    // pre-format the global tags in `statsd` format, including a leading `|#`.
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

/// Invoke a callback with the current [`MetricsWrapper`] and [`LocalAggregator`].
///
/// If metrics have not been configured, the callback is not invoked.
/// For the most part the [`metric!`](crate::metric) macro should be used instead.
#[inline(always)]
pub fn with_client<F>(f: F)
where
    F: FnOnce(&MetricsWrapper, &mut LocalAggregator),
{
    if let Some(client) = METRICS_CLIENT.get() {
        client.with_local_aggregator(f)
    }
}

/// Emits a metric.
#[macro_export]
macro_rules! metric {
    // counters
    (counter($id:expr) += $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|_client, local| {
            let tags: &[(&str, &str)] = &[
                $(($k, $v)),*
            ];
            local.emit_count($id, $value, tags);
        });
    }};

    // gauges
    (gauge($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|_client, local| {
            let tags: &[(&str, &str)] = &[
                $(($k, $v)),*
            ];
            local.emit_gauge($id, $value, tags);
        });
    }};

    // timers
    (timer($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|client, _local| {
            use $crate::metrics::prelude::*;
            client
                .time_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // we use statsd timers to send things such as filesizes as well.
    (time_raw($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|client, _local| {
            use $crate::metrics::prelude::*;
            client
                .time_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};

    // histograms
    (histogram($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|client, _local| {
            use $crate::metrics::prelude::*;
            client
                .histogram_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        });
    }};
}
