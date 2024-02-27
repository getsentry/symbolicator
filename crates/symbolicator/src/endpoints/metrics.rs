use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::http::{Response, StatusCode};
use tower_layer::Layer;
use tower_service::Service as TowerService;

use crate::metric;

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
            metric!(
                counter("responses.status_code") += 1,
                "status" => status.as_str(),
            );
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
