use std::fmt;

use actix_web::dev::{Body, ResponseBody, Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header;
use actix_web::Error;
use failure::Fail;
use futures::{future, Future, Poll};
use serde::{Deserialize, Serialize};

use crate::server::ServerError;
use crate::utils::futures::ResultFuture;

/// An error response from an api.
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ApiErrorResponse {
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    causes: Option<Vec<String>>,
}

impl ApiErrorResponse {
    /// Creates an error response with a detail message.
    pub fn with_detail<E: fmt::Display>(error: E) -> Self {
        Self {
            detail: Some(error.to_string()),
            causes: None,
        }
    }

    /// Creates an error response from a `Fail`.
    pub fn from_fail(fail: &dyn Fail) -> Self {
        let mut messages = vec![];

        for cause in Fail::iter_chain(fail) {
            let msg = cause.to_string();
            if !messages.contains(&msg) {
                messages.push(msg);
            }
        }

        Self {
            detail: Some(messages.remove(0)),
            causes: if messages.is_empty() {
                None
            } else {
                Some(messages)
            },
        }
    }
}

/// An error handler that serializes server errors to JSON.
pub struct ErrorHandler;

impl<S, B> Transform<S> for ErrorHandler
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = ErrorHandlerMiddleware<S>;
    type Future = future::FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(ErrorHandlerMiddleware(service))
    }
}

pub struct ErrorHandlerMiddleware<S>(S);

impl<S, B> Service for ErrorHandlerMiddleware<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = ResultFuture<ServiceResponse<B>, Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, request: ServiceRequest) -> Self::Future {
        let future = self.0.call(request).and_then(|response| {
            let error = match response.response().error() {
                Some(error) => error,
                None => return response,
            };

            let error_response = match error.as_error::<ServerError>() {
                Some(server_error) => ApiErrorResponse::from_fail(server_error),
                None => ApiErrorResponse::with_detail(error.to_string()),
            };

            response.map_body(|head, _body| {
                head.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("application/json"),
                );

                let body = serde_json::to_string(&error_response)
                    .unwrap_or_else(|_| r#"{"detail":"internal server error"}"#.to_owned());

                ResponseBody::Other(Body::from(body))
            })
        });

        Box::new(future)
    }
}
