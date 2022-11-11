use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use sentry::integrations::tower::{NewSentryLayer, SentryHttpLayer};
use tower::ServiceBuilder;

use crate::service::RequestService;

mod applecrashreport;
mod error;
mod metrics;
mod minidump;
mod multipart;
mod proxy;
mod requests;
mod symbolicate;

pub use error::ResponseError;
use metrics::MetricsLayer;

use self::minidump::handle_minidump_request as minidump;
use applecrashreport::handle_apple_crash_report_request as applecrashreport;
use proxy::proxy_symstore_request as proxy;
use requests::poll_request as requests;
use symbolicate::symbolicate_frames as symbolicate;

pub async fn healthcheck() -> &'static str {
    crate::metric!(counter("healthcheck") += 1);
    "ok"
}

pub fn create_app(service: RequestService) -> Router {
    // The layers here go "top to bottom" according to the reading order here.
    let layer = ServiceBuilder::new()
        .layer(axum::extract::Extension(service))
        .layer(NewSentryLayer::new_from_top())
        .layer(SentryHttpLayer::with_transaction())
        .layer(MetricsLayer)
        .layer(DefaultBodyLimit::disable());
    // XXX: Adding a limit would lead to a confusing trait error that I don't know how to solve:
    // > the trait `tower_service::Service<axum::http::Request<http_body::limited::Limited<_>>>` is not implemented for `Route`
    // .layer(RequestBodyLimitLayer::new(100 * 1024 * 1024)) // ~100MB;
    Router::new()
        .route("/proxy/*path", get(proxy).head(proxy))
        .route("/requests/:request_id", get(requests))
        .route("/applecrashreport", post(applecrashreport))
        .route("/minidump", post(minidump))
        .route("/symbolicate", post(symbolicate))
        .layer(layer)
        // the healthcheck is last, as it will bypass all the middlewares
        .route("/healthcheck", get(healthcheck))
}
