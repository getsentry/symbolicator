//! Provides access to the metrics sytem.
use std::net::ToSocketAddrs;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use cadence::{Metric, MetricBuilder, StatsdClient, UdpMetricSink};
use parking_lot::RwLock;

type Hostname = String;
type HostnameTag = String;

lazy_static::lazy_static! {
    static ref METRICS_CLIENT: RwLock<Option<Arc<MetricsClient>>> = RwLock::new(None);
}

thread_local! {
    static CURRENT_CLIENT: Option<Arc<MetricsClient>> = METRICS_CLIENT.read().clone();
}

/// Internal prelude for the macro
#[doc(hidden)]
pub mod _pred {
    pub use cadence::prelude::*;
    pub use std::time::Instant;
}

/// The metrics prelude that is necessary to use the client.
pub mod prelude {
    pub use cadence::prelude::*;
}

#[derive(Debug)]
pub struct MetricsClient {
    /// The raw statsd client.
    pub statsd_client: StatsdClient,
    /// The hostname and the tag to report it to.
    pub hostname: Option<(HostnameTag, Hostname)>,
}

impl MetricsClient {
    #[inline(always)]
    pub fn send_metric<'a, T>(&'a self, mut metric: MetricBuilder<'a, '_, T>)
    where
        T: Metric + From<String>,
    {
        if let Some((tag, name)) = self.hostname.as_ref() {
            metric = metric.with_tag(tag, name);
        }
        metric.send()
    }
}

impl Deref for MetricsClient {
    type Target = StatsdClient;

    fn deref(&self) -> &Self::Target {
        &self.statsd_client
    }
}

impl DerefMut for MetricsClient {
    fn deref_mut(&mut self) -> &mut StatsdClient {
        &mut self.statsd_client
    }
}

/// Set a new statsd client.
pub fn set_client(client: MetricsClient) {
    *METRICS_CLIENT.write() = Some(Arc::new(client));
}

/// Tell the metrics system to report to statsd.
pub fn configure_statsd<A: ToSocketAddrs>(
    prefix: &str,
    host: A,
    hostname: Option<(HostnameTag, Hostname)>,
) {
    let addrs: Vec<_> = host.to_socket_addrs().unwrap().collect();
    if !addrs.is_empty() {
        log::info!("Reporting metrics to statsd at {}", addrs[0]);
    }
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let sink = UdpMetricSink::from(&addrs[..], socket).unwrap();
    let statsd_client = StatsdClient::from_sink(prefix, sink);
    set_client(MetricsClient {
        statsd_client,
        hostname,
    });
}

/// Invoke a callback with the current statsd client.
///
/// If statsd is not configured the callback is not invoked. For the most part
/// the [`metric!`] macro should be used instead.
#[inline(always)]
pub fn with_client<F, R>(f: F) -> R
where
    F: FnOnce(&MetricsClient) -> R,
    R: Default,
{
    CURRENT_CLIENT.with(|client| {
        if let Some(client) = client {
            f(&*client)
        } else {
            Default::default()
        }
    })
}

/// Emits a metric.
#[macro_export]
macro_rules! metric {
    // counters
    (counter($id:expr) += $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.count_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};
    (counter($id:expr) -= $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.count_with_tags($id, -$value)
                    $(.with_tag($k, $v))*
             );
        })
    }};

    // gauges
    (gauge($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.gauge_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};

    // timers
    (timer($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.time_duration_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};

    // we use statsd timers to send things such as filesizes as well.
    (time_raw($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.time_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};

    // histograms
    (histogram($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.histogram_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};
}

macro_rules! future_metrics {
    // Collect generic metrics about a future
    ($task_name:expr, $timeout:expr, $future:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use std::time::Instant;
        use futures01::future::{self, Either, Future};
        use tokio01::prelude::FutureExt;

        let creation_time = Instant::now();

        future::lazy(move || {
            metric!(
                timer("futures.wait_time") = creation_time.elapsed(),
                "task_name" => $task_name,
                $($k => $v,)*
            );
            let start_time = Instant::now();

            let fut = $future;

            let fut = if let Some((timeout, timeout_e)) = $timeout {
                Either::A(fut.timeout(timeout).map_err(move |e| {
                    e.into_inner().unwrap_or_else(|| {
                        metric!(
                            timer("futures.done") = start_time.elapsed(),
                            "task_name" => $task_name,
                            "status" => "timeout",
                            $($k => $v,)*
                        );
                        timeout_e
                    })
                }))
            } else {
                Either::B(fut)
            };

            fut.then(move |result| {
                metric!(
                    timer("futures.done") = start_time.elapsed(),
                    "task_name" => $task_name,
                    "status" => match result {
                        Ok(_) => "ok",
                        Err(_) => "err",
                    },
                    $($k => $v,)*
                );
                result
            })
        })
    }};
}
