//! Provides access to the metrics sytem.
use std::collections::BTreeMap;
use std::fmt::Write;
use std::net::ToSocketAddrs;
use std::ops::Deref;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use cadence::{BufferedUdpMetricSink, MetricSink, QueuingMetricSink, StatsdClient};
use crossbeam_utils::CachePadded;
use rustc_hash::FxHashMap;
use thread_local::ThreadLocal;

static METRICS_CLIENT: OnceLock<MetricsWrapper> = OnceLock::new();

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
#[derive(Debug)]
pub struct MetricsWrapper {
    /// The raw `cadence` client.
    statsd_client: StatsdClient,

    /// A thread local aggregator.
    local_aggregator: LocalAggregators,
}

impl MetricsWrapper {
    /// Invokes the provided callback with a mutable reference to a thread-local [`LocalAggregator`].
    fn with_local_aggregator(&self, f: impl FnOnce(&mut LocalAggregator)) {
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

/// We are not (yet) aggregating distributions, but keeping every value.
/// To not overwhelm downstream services, we send them in batches instead of all at once.
const DISTRIBUTION_BATCH_SIZE: usize = 1;

/// The interval in which to flush out metrics.
/// NOTE: In particular for timer metrics, we have observed that for some reason, *some* of the timer
/// metrics are getting lost (interestingly enough, not all of them) and are not being aggregated into the `.count`
/// sub-metric collected by `veneur`. Lets just flush a lot more often in order to emit less metrics per-flush.
const SEND_INTERVAL: Duration = Duration::from_millis(125);

/// Creates [`LocalAggregators`] and starts a thread that will periodically
/// send aggregated metrics upstream to the `sink`.
fn make_aggregator(prefix: &str, formatted_global_tags: String, sink: Sink) -> LocalAggregators {
    let local_aggregators = LocalAggregators::default();

    let aggregators = Arc::clone(&local_aggregators);
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{}.", prefix.trim_end_matches('.'))
    };

    let thread_fn = move || {
        // to avoid reallocation, just reserve some space.
        // the size is rather arbitrary, but should be large enough for formatted metrics.
        let mut formatted_metric = String::with_capacity(256);
        let mut suffix = String::with_capacity(128);

        loop {
            thread::sleep(SEND_INTERVAL);

            let (total_counters, total_distributions) = aggregate_all(&aggregators);

            // send all the aggregated "counter like" metrics
            for (AggregationKey { ty, name, tags }, value) in total_counters {
                formatted_metric.push_str(&prefix);
                formatted_metric.push_str(name);

                let _ = write!(&mut formatted_metric, ":{value}{ty}{formatted_global_tags}");

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

            // send all the aggregated "distribution like" metrics
            // we do this in a batched manner, as we do not actually *aggregate* them,
            // but still send each value individually.
            for (AggregationKey { ty, name, tags }, value) in total_distributions {
                suffix.push_str(&formatted_global_tags);
                if let Some(tags) = tags {
                    if formatted_global_tags.is_empty() {
                        suffix.push_str("|#");
                    } else {
                        suffix.push(',');
                    }
                    suffix.push_str(&tags);
                }

                for batch in value.chunks(DISTRIBUTION_BATCH_SIZE) {
                    formatted_metric.push_str(&prefix);
                    formatted_metric.push_str(name);

                    for value in batch {
                        let _ = write!(&mut formatted_metric, ":{value}");
                    }

                    formatted_metric.push_str(ty);
                    formatted_metric.push_str(&suffix);

                    let _ = sink.emit(&formatted_metric);
                    formatted_metric.clear();
                }

                suffix.clear();
            }
        }
    };

    thread::Builder::new()
        .name("metrics-aggregator".into())
        .spawn(thread_fn)
        .unwrap();

    local_aggregators
}

fn aggregate_all(aggregators: &LocalAggregators) -> (AggregatedCounters, AggregatedDistributions) {
    let mut total_counters = AggregatedCounters::default();
    let mut total_distributions = AggregatedDistributions::default();

    for local_aggregator in aggregators.iter() {
        let (local_counters, local_distributions) = {
            let mut local_aggregator = local_aggregator.lock().unwrap();
            (
                std::mem::take(&mut local_aggregator.aggregated_counters),
                std::mem::take(&mut local_aggregator.aggregated_distributions),
            )
        };

        // aggregate all the "counter like" metrics
        if total_counters.is_empty() {
            total_counters = local_counters;
        } else {
            for (key, value) in local_counters {
                let ty = key.ty;
                let aggregated_value = total_counters.entry(key).or_default();
                if ty == "|c" {
                    *aggregated_value += value;
                } else if ty == "|g" {
                    // FIXME: when aggregating multiple thread-locals,
                    // we don’t really know which one is the "latest".
                    // But it also does not really matter that much?
                    *aggregated_value = value;
                }
            }
        }

        // aggregate all the "distribution like" metrics
        if total_distributions.is_empty() {
            total_distributions = local_distributions;
        } else {
            for (key, value) in local_distributions {
                let aggregated_value = total_distributions.entry(key).or_default();
                aggregated_value.extend(value);
            }
        }
    }

    (total_counters, total_distributions)
}

/// The key by which we group/aggregate metrics.
#[derive(Eq, Ord, PartialEq, PartialOrd, Hash, Debug)]
struct AggregationKey {
    /// The metric type, pre-formatted as a statsd suffix such as `|c`.
    ty: &'static str,
    /// The name of the metric.
    name: &'static str,
    /// The metric tags, pre-formatted as a statsd suffix, excluding the `|#` prefix.
    tags: Option<Box<str>>,
}

type AggregatedCounters = FxHashMap<AggregationKey, i64>;
type AggregatedDistributions = FxHashMap<AggregationKey, Vec<f64>>;

pub trait IntoDistributionValue {
    fn into_value(self) -> f64;
}

impl IntoDistributionValue for Duration {
    fn into_value(self) -> f64 {
        self.as_secs_f64() * 1_000.
    }
}

impl IntoDistributionValue for usize {
    fn into_value(self) -> f64 {
        self as f64
    }
}

impl IntoDistributionValue for u64 {
    fn into_value(self) -> f64 {
        self as f64
    }
}

impl IntoDistributionValue for i32 {
    fn into_value(self) -> f64 {
        self as f64
    }
}

/// The `thread_local` aggregator which pre-aggregates metrics per-thread.
#[derive(Default, Debug)]
pub struct LocalAggregator {
    /// A mutable scratch-buffer that is reused to format tags into it.
    buf: String,
    /// A map of all the `counter` and `gauge` metrics we have aggregated thus far.
    aggregated_counters: AggregatedCounters,
    /// A map of all the `timer` and `histogram` metrics we have aggregated thus far.
    aggregated_distributions: AggregatedDistributions,
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

    /// Emit a `timer` metric, for which every value is accumulated
    pub fn emit_timer(&mut self, name: &'static str, value: f64, tags: &[(&'static str, &str)]) {
        let tags = self.format_tags(tags);
        self.emit_distribution_inner("|ms", name, value, tags)
    }

    /// Emit a `histogram` metric, for which every value is accumulated
    pub fn emit_histogram(
        &mut self,
        name: &'static str,
        value: f64,
        tags: &[(&'static str, &str)],
    ) {
        let tags = self.format_tags(tags);
        self.emit_distribution_inner("|h", name, value, tags)
    }

    /// Emit a distribution metric, which is aggregated by appending to a list of values.
    fn emit_distribution_inner(
        &mut self,
        ty: &'static str,
        name: &'static str,
        value: f64,
        tags: Option<Box<str>>,
    ) {
        let key = AggregationKey { ty, name, tags };

        let aggregation = self.aggregated_distributions.entry(key).or_default();
        aggregation.push(value);
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
    F: FnOnce(&mut LocalAggregator),
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
        $crate::metrics::with_client(|local| {
            let tags: &[(&'static str, &str)] = &[
                $(($k, $v)),*
            ];
            local.emit_count($id, $value, tags);
        });
    }};

    // gauges
    (gauge($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|local| {
            let tags: &[(&'static str, &str)] = &[
                $(($k, $v)),*
            ];
            local.emit_gauge($id, $value, tags);
        });
    }};

    // timers
    (timer($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|local| {
            let tags: &[(&'static str, &str)] = &[
                $(($k, $v)),*
            ];
            use $crate::metrics::IntoDistributionValue;
            local.emit_timer($id, ($value).into_value(), tags);
        });
    }};

    // we use statsd timers to send things such as filesizes as well.
    (time_raw($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|local| {
            let tags: &[(&'static str, &str)] = &[
                $(($k, $v)),*
            ];
            use $crate::metrics::IntoDistributionValue;
            local.emit_timer($id, ($value).into_value(), tags);
        });
    }};

    // histograms
    (histogram($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        $crate::metrics::with_client(|local| {
            let tags: &[(&'static str, &str)] = &[
                $(($k, $v)),*
            ];
            use $crate::metrics::IntoDistributionValue;
            local.emit_histogram($id, ($value).into_value(), tags);
        });
    }};
}
