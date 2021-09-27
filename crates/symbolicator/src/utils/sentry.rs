use std::collections::BTreeMap;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::Request;
use tower_layer::Layer;
use tower_service::Service;

/// Write own data to [`sentry::Scope`], only the subset that is considered useful for debugging.
pub trait ConfigureScope {
    /// Writes information to the given scope.
    fn to_scope(&self, scope: &mut sentry::Scope);

    /// Configures the current scope.
    fn configure_scope(&self) {
        sentry::configure_scope(|scope| self.to_scope(scope));
    }
}

#[derive(Clone)]
pub struct SentryRequestLayer;

#[derive(Clone)]
pub struct SentryRequestService<S> {
    service: S,
}

impl<S> Layer<S> for SentryRequestLayer {
    type Service = SentryRequestService<S>;

    fn layer(&self, service: S) -> Self::Service {
        Self::Service { service }
    }
}

impl<S> Service<Request<Body>> for SentryRequestService<S>
where
    S: Service<Request<Body>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        sentry::configure_scope(|scope| {
            let mut headers = BTreeMap::new();
            for (header, value) in request.headers() {
                headers.insert(
                    header.to_string(),
                    value.to_str().unwrap_or("<Opaque header value>").into(),
                );
            }
            scope.set_context(
                "HTTP Request Headers",
                sentry::protocol::Context::Other(headers),
            );
        });
        self.service.call(request)
    }
}
