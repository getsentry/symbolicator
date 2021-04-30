//! Provides access to the metrics sytem.
use std::net::ToSocketAddrs;
use std::sync::Arc;

use cadence::StatsdClient;
use parking_lot::RwLock;

lazy_static::lazy_static! {
    static ref METRICS_CLIENT: RwLock<Option<Arc<StatsdClient>>> = RwLock::new(None);
}

thread_local! {
    static CURRENT_CLIENT: Option<Arc<StatsdClient>> = METRICS_CLIENT.read().clone();
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

/// Set a new statsd client.
pub fn set_client(statsd_client: StatsdClient) {
    *METRICS_CLIENT.write() = Some(Arc::new(statsd_client));
}

/// Tell the metrics system to report to statsd.
pub fn configure_statsd<A: ToSocketAddrs>(prefix: &str, host: A) {
    let addrs: Vec<_> = host.to_socket_addrs().unwrap().collect();
    if !addrs.is_empty() {
        log::info!("Reporting metrics to statsd at {}", addrs[0]);
    }
    set_client(StatsdClient::from_udp_host(prefix, &addrs[..]).unwrap());
}

/// Invoke a callback with the current statsd client.
///
/// If statsd is not configured the callback is not invoked. For the most part
/// the [`metric!`] macro should be used instead.
#[inline(always)]
pub fn with_client<F, R>(f: F) -> R
where
    F: FnOnce(&StatsdClient) -> R,
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
            client.count_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        })
    }};
    (counter($id:expr) -= $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.count_with_tags($id, -$value)
                $(.with_tag($k, $v))*
                .send();
        })
    }};

    // gauges
    (gauge($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.gauge_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        })
    }};

    // timers
    (timer($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.time_duration_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
        })
    }};
    (timer($id:expr), $block:block $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        let now = Instant::now();
        let rv = {$block};
        $crate::metrics::with_client(|client| {
            client.time_duration_with_tags($id, now.elapsed())
                $(.with_tag($k, $v))*
                .send();
        });
        rv
    }};

    // we use statsd timers to send things such as filesizes as well.
    (time_raw($id:expr) = $value:expr $(, $k:expr => $v:expr)* $(,)?) => {{
        use $crate::metrics::_pred::*;
        $crate::metrics::with_client(|client| {
            client.time_with_tags($id, $value)
                $(.with_tag($k, $v))*
                .send();
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
