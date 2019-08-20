use std::time::Instant;

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::Error;
use futures::{future, Future, Poll};

use crate::utils::futures::ResultFuture;

/// Middleware for timing request durations.
#[derive(Clone, Debug, Default)]
pub struct RequestMetrics;

impl<S, B> Transform<S> for RequestMetrics
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = RequestMetricsMiddleware<S>;
    type Future = future::FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(RequestMetricsMiddleware { service })
    }
}

pub struct RequestMetricsMiddleware<S> {
    service: S,
}

impl<S, B> Service for RequestMetricsMiddleware<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = ResultFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, request: ServiceRequest) -> Self::Future {
        let exclude_path = request.path() == "/healthcheck";
        let start_time = Instant::now();

        let inner = self.service.call(request);
        Box::new(inner.inspect(move |response| {
            if !exclude_path {
                metric!(timer("requests.duration") = start_time.elapsed());
                metric!(counter(&format!("responses.status_code.{}", response.status())) += 1);
            }
        }))
    }
}
