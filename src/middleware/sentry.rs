use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header::HeaderName;
use actix_web::{Error, ResponseError};
use futures::{future, Future, Poll};
use sentry::Hub;
use uuid::Uuid;

use crate::utils::sentry::SentryFutureExt;

#[doc(hidden)]
#[allow(unused)]
#[derive(Debug)]
pub struct NoError;

impl fmt::Display for NoError {
    fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
        Ok(())
    }
}

impl ResponseError for NoError {}

pub struct Sentry<E> {
    reporter: Rc<dyn Fn(&E) -> Uuid>,
}

impl Sentry<NoError> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<E> Sentry<E> {
    pub fn error_reporter<N, F>(self, handler: F) -> Sentry<N>
    where
        N: ResponseError + 'static,
        F: Fn(&N) -> Uuid + 'static,
    {
        Sentry {
            reporter: Rc::new(handler),
        }
    }
}

impl Default for Sentry<NoError> {
    fn default() -> Self {
        Self {
            reporter: Rc::new(|_| Uuid::nil()),
        }
    }
}

impl<E, S, B> Transform<S> for Sentry<E>
where
    E: ResponseError + 'static,
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = SentryMiddleware<E, S>;
    type Future = future::FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(SentryMiddleware {
            service,
            reporter: self.reporter.clone(),
        })
    }
}

pub struct SentryMiddleware<E, S> {
    service: S,
    reporter: Rc<dyn Fn(&E) -> Uuid>,
}

impl<E, S, B> Service for SentryMiddleware<E, S>
where
    E: ResponseError + 'static,
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = Box<dyn Future<Item = ServiceResponse<B>, Error = Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, request: ServiceRequest) -> Self::Future {
        let hub = Arc::new(Hub::new_from_top(Hub::main()));
        let reporter = self.reporter.clone();

        // TODO(ja): Port everything over from sentry-actix

        let response_future = Hub::run(hub.clone(), || self.service.call(request))
            .then(move |mut result| {
                // Check if we want to handle this error depending on the status code. Only handle
                // internal server errors (status code 5XX).
                let status = match result {
                    Ok(ref response) => response.status(),
                    Err(ref error) => error.as_response_error().error_response().status(),
                };

                if !status.is_server_error() {
                    return result;
                }

                // Extract the raw error from this response. This can be one of two cases:
                //  1. The error is stored in the service response. This is the case when an error
                //     is returned from a service endpoint and actix wraps it in a response.
                //  2. The future resolved to an error. This is usually the case for actix-internal
                //     errors.
                let error_opt = match result {
                    Ok(ref response) => response.response().error(),
                    Err(ref error) => Some(error),
                };

                let error = match error_opt {
                    Some(error) => error,
                    None => return result,
                };

                // Try to handle the error using the configured reporter. If the error is of a
                // different type, use the generic capture_message.
                let event_id = match error_opt.and_then(|e| e.as_error::<E>()) {
                    Some(error) => reporter(error),
                    None => sentry::capture_message(&error.to_string(), sentry::Level::Error),
                };

                // If configured, write the header back into the service response. This is only
                // possible if the future did not resolve to an error. The error will still be
                // converted in to a response, but that happens at a later time. To ensure that
                // subsequent middlewares still see the actix error, we leave that case out.
                if let Ok(ref mut response) = result {
                    response.response_mut().headers_mut().insert(
                        HeaderName::from_static("x-sentry-event"),
                        event_id.to_simple_ref().to_string().parse().unwrap(),
                    );
                }

                result
            })
            .bind_hub(hub);

        Box::new(response_future)
    }
}
