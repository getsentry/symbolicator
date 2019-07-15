use std::sync::Arc;

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::Error;
use futures::{future, Poll};
use sentry::Hub;

use crate::utils::sentry::SentryFuture;

pub struct Sentry;

impl<S, B> Transform<S> for Sentry
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = SentryMiddleware<S>;
    type Future = future::FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(SentryMiddleware { service })
    }
}

pub struct SentryMiddleware<S> {
    service: S,
}

impl<S, B, E> Service for SentryMiddleware<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = E>,
    S::Future: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = E;
    type Future = SentryFuture<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, request: ServiceRequest) -> Self::Future {
        let hub = Arc::new(Hub::new_from_top(Hub::main()));
        // TODO(ja): Port everything over from sentry-actix
        Hub::run(hub.clone(), || {
            SentryFuture::new(hub, self.service.call(request))
        })
    }
}
