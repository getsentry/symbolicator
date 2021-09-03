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
            let (transaction, req) = extract_request(&request);

            scope.add_event_processor(Box::new(move |mut event| {
                if event.transaction.is_none() {
                    event.transaction = transaction.clone();
                }
                if event.request.is_none() {
                    event.request = Some(req.clone());
                }
                Some(event)
            }))
        });
        self.service.call(request)
    }
}

fn extract_request(req: &Request<Body>) -> (Option<String>, sentry::protocol::Request) {
    let transaction = Some(req.uri().path().to_string());
    let sentry_req = sentry::protocol::Request {
        url: req.uri().to_string().parse().ok(),
        method: Some(req.method().to_string()),
        headers: req
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().into(), v.to_str().unwrap_or("").into()))
            .collect(),
        ..Default::default()
    };

    (transaction, sentry_req)
}
