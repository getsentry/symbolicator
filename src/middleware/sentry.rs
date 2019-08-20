use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header::HeaderName;
use actix_web::{Error, ResponseError};
use futures::{future, Future, Poll};
use sentry::protocol::{ClientSdkPackage, Request};
use sentry::Hub;
use uuid::Uuid;

use crate::utils::futures::ResultFuture;
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
    emit_header: bool,
    capture_server_errors: bool,
    reporter: Rc<dyn Fn(&E) -> Uuid>,
}

impl Sentry<NoError> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<E> Sentry<E> {
    pub fn emit_header(mut self, emit_header: bool) -> Self {
        self.emit_header = emit_header;
        self
    }

    pub fn capture_server_errors(mut self, capture_server_errors: bool) -> Self {
        self.capture_server_errors = capture_server_errors;
        self
    }

    pub fn error_reporter<N, F>(self, handler: F) -> Sentry<N>
    where
        N: ResponseError + 'static,
        F: Fn(&N) -> Uuid + 'static,
    {
        Sentry {
            emit_header: self.emit_header,
            capture_server_errors: self.capture_server_errors,
            reporter: Rc::new(handler),
        }
    }
}

impl Default for Sentry<NoError> {
    fn default() -> Self {
        Self {
            emit_header: false,
            capture_server_errors: true,
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
            emit_header: self.emit_header,
            capture_server_errors: self.capture_server_errors,
            reporter: self.reporter.clone(),
        })
    }
}

fn extract_request(
    service_request: &ServiceRequest,
    include_pii: bool,
) -> (Option<String>, Request) {
    let connection_info = service_request.connection_info();

    // Actix Web 1.0 does not expose routes in a way where we can extract a transaction.
    let transaction = None;

    let mut request = Request {
        url: format!(
            "{}://{}{}",
            connection_info.scheme(),
            connection_info.host(),
            service_request.uri()
        )
        .parse()
        .ok(),
        method: Some(service_request.method().to_string()),
        headers: service_request
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().into(), v.to_str().unwrap_or("").into()))
            .collect(),
        ..Request::default()
    };

    if let Some(remote) = connection_info.remote().filter(|_| include_pii) {
        request.env.insert("REMOTE_ADDR".into(), remote.into());
    }

    (transaction, request)
}

pub struct SentryMiddleware<E, S> {
    service: S,
    emit_header: bool,
    capture_server_errors: bool,
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
    type Future = ResultFuture<ServiceResponse<B>, Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, request: ServiceRequest) -> Self::Future {
        let hub = Arc::new(Hub::new_from_top(Hub::main()));

        let emit_header = self.emit_header;
        let capture_server_errors = self.capture_server_errors;
        let reporter = self.reporter.clone();

        let include_pii = hub
            .client()
            .as_ref()
            .map_or(false, |x| x.options().send_default_pii);
        let (transaction, sentry_request) = extract_request(&request, include_pii);

        hub.configure_scope(move |scope| {
            scope.add_event_processor(Box::new(move |mut event| {
                event.transaction = event.transaction.or_else(|| transaction.clone());
                event.request.get_or_insert_with(|| sentry_request.clone());

                if let Some(ref mut sdk) = event.sdk {
                    sdk.to_mut().packages.push(ClientSdkPackage {
                        name: "sentry-actix".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    });
                }

                Some(event)
            }));
        });

        let response_future = Hub::run(hub.clone(), || self.service.call(request))
            .then(move |mut result| {
                if !capture_server_errors {
                    return result;
                }

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
                let event_id = match error.as_error::<E>() {
                    Some(error) => reporter(error),
                    None => sentry::capture_message(&error.to_string(), sentry::Level::Error),
                };

                // If configured, write the header back into the service response. This is only
                // possible if the future did not resolve to an error. The error will still be
                // converted in to a response, but that happens at a later time. To ensure that
                // subsequent middlewares still see the actix error, we leave that case out.
                if emit_header {
                    if let Ok(ref mut response) = result {
                        response.response_mut().headers_mut().insert(
                            HeaderName::from_static("x-sentry-event"),
                            event_id.to_simple_ref().to_string().parse().unwrap(),
                        );
                    }
                }

                result
            })
            .bind_hub(hub);

        Box::new(response_future)
    }
}
