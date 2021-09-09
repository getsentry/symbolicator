//! Provides access to the metrics sytem.
use std::collections::BTreeMap;
use std::future::Future;
use std::net::ToSocketAddrs;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::http::{Response, StatusCode};
use cadence::{Metric, MetricBuilder, StatsdClient, UdpMetricSink};
use parking_lot::RwLock;
use tower_layer::Layer;
use tower_service::Service as TowerService;

lazy_static::lazy_static! {
    static ref METRICS_CLIENT: RwLock<Option<Arc<MetricsClient>>> = RwLock::new(None);
}

thread_local! {
    static CURRENT_CLIENT: Option<Arc<MetricsClient>> = METRICS_CLIENT.read().clone();
}

/// The metrics prelude that is necessary to use the client.
pub mod prelude {
    pub use cadence::prelude::*;
}

#[derive(Debug)]
pub struct MetricsClient {
    /// The raw statsd client.
    pub statsd_client: StatsdClient,

    /// A collection of tags and values that will be sent with every metric.
    tags: BTreeMap<String, String>,
}

impl MetricsClient {
    #[inline(always)]
    pub fn send_metric<'a, T>(&'a self, mut metric: MetricBuilder<'a, '_, T>)
    where
        T: Metric + From<String>,
    {
        for (tag, value) in self.tags.iter() {
            metric = metric.with_tag(tag, value);
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
pub fn configure_statsd<A: ToSocketAddrs>(prefix: &str, host: A, tags: BTreeMap<String, String>) {
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
        tags,
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
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.count_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};
    (counter($id:expr) -= $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.count_with_tags($id, -$value)
                    $(.with_tag($k, $v))*
             );
        })
    }};

    // gauges
    (gauge($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.gauge_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};

    // timers
    (timer($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.time_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};

    // we use statsd timers to send things such as filesizes as well.
    (time_raw($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.time_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};

    // histograms
    (histogram($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::prelude::*;
        $crate::metrics::with_client(|client| {
            client.send_metric(
                client.histogram_with_tags($id, $value)
                    $(.with_tag($k, $v))*
            );
        })
    }};
}

#[derive(Clone)]
pub struct MetricsLayer;

#[derive(Clone)]
pub struct MetricsService<S> {
    service: S,
}

pub struct MetricsFuture<F> {
    start: Instant,
    future: F,
}

impl<F, B, E> Future for MetricsFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let start = self.start;
        // https://doc.rust-lang.org/std/pin/index.html#pinning-is-structural-for-field
        let future = unsafe { self.map_unchecked_mut(|s| &mut s.future) };
        let poll = future.poll(cx);
        if let Poll::Ready(ref res) = poll {
            metric!(timer("requests.duration") = start.elapsed());
            let status = res
                .as_ref()
                .map(|r| r.status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            metric!(counter(&format!("responses.status_code.{}", status)) += 1);
        }
        poll
    }
}

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, service: S) -> Self::Service {
        Self::Service { service }
    }
}

impl<S, Request, B> TowerService<Request> for MetricsService<S>
where
    S: TowerService<Request, Response = Response<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = MetricsFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        Self::Future {
            start: Instant::now(),
            future: self.service.call(request),
        }
    }
}
