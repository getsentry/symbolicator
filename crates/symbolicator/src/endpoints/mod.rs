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
mod sourcemap;
mod symbolicate;

pub use error::ResponseError;
use metrics::MetricsLayer;

use self::minidump::handle_minidump_request as minidump;
use applecrashreport::handle_apple_crash_report_request as applecrashreport;
use proxy::proxy_symstore_request as proxy;
use requests::poll_request as requests;
use sourcemap::handle_sourcemap_request as sourcemap;
use symbolicate::symbolicate_frames as symbolicate;

pub async fn healthcheck() -> &'static str {
    crate::metric!(counter("healthcheck") += 1);
    "ok"
}

pub fn create_app(service: RequestService) -> Router {
    // The layers here go "top to bottom" according to the reading order here.
    let layer = ServiceBuilder::new()
        .layer(NewSentryLayer::new_from_top())
        .layer(SentryHttpLayer::with_transaction())
        .layer(MetricsLayer)
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024));
    // We have a global 100M body limit, but a 5M symbolicate body limit
    let symbolicate_route = post(symbolicate).layer(DefaultBodyLimit::max(5 * 1024 * 1024));
    Router::new()
        .route("/proxy/*path", get(proxy).head(proxy))
        .route("/requests/:request_id", get(requests))
        .route("/applecrashreport", post(applecrashreport))
        .route("/minidump", post(minidump))
        // TODO(sourcemap): Verify whether this is the endpoint name we actually want to use.
        .route("/sourcemap", post(sourcemap))
        .route("/symbolicate", symbolicate_route)
        .with_state(service)
        .layer(layer)
        // the healthcheck is last, as it will bypass all the middlewares
        .route("/healthcheck", get(healthcheck))
}
