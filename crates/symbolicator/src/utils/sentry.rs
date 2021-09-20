use std::collections::BTreeMap;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Request};
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
            let mut sentry_headers = BTreeMap::new();
            let headers = request.headers();
            fn get_str(val: &HeaderValue) -> Option<&str> {
                val.to_str().ok()
            }
            if let Some(worker_id) = headers.get("X-Sentry-Worker-Id").and_then(get_str) {
                sentry_headers.insert(String::from("X-Sentry-Worker-Id"), worker_id.into());
            }
            if let Some(project_id) = headers.get("X-Sentry-Project-Id").and_then(get_str) {
                sentry_headers.insert(String::from("X-Sentry-Project-Id"), project_id.into());
            }
            if let Some(event_id) = headers.get("X-Sentry-Event-Id").and_then(get_str) {
                sentry_headers.insert(String::from("X-Sentry-Event-Id"), event_id.into());
            }

            // TODO: We can use this in the future for distributed tracing once we make
            // properly support that in the SDK.
            if let Some(trace_id) = headers.get("Sentry-Trace").and_then(get_str) {
                sentry_headers.insert(String::from("Sentry-Trace"), trace_id.into());
            }

            scope.set_context(
                "Sentry HTTP Headers",
                sentry::protocol::Context::Other(sentry_headers),
            );
        });
        self.service.call(request)
    }
}
