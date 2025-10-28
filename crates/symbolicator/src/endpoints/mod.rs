use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
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
mod symbolicate_any;
mod symbolicate_js;
mod symbolicate_jvm;

pub use error::ResponseError;
use metrics::MetricsLayer;

use self::minidump::handle_minidump_request as minidump;
use applecrashreport::handle_apple_crash_report_request as applecrashreport;
use proxy::proxy_symstore_request as proxy;
use requests::poll_request as requests;
use symbolicate::symbolicate_frames as symbolicate;
use symbolicate_any::symbolicate_any;
use symbolicate_js::handle_symbolication_request as symbolicate_js;
use symbolicate_jvm::handle_symbolication_request as symbolicate_jvm;

pub async fn healthcheck() -> &'static str {
    crate::metric!(counter("healthcheck") += 1);
    "ok"
}

pub fn create_app(service: RequestService) -> Router {
    // The layers here go "top to bottom" according to the reading order here.
    let layer = ServiceBuilder::new()
        .layer(NewSentryLayer::new_from_top())
        .layer(SentryHttpLayer::new().enable_transaction())
        .layer(MetricsLayer)
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024));
    // We have a global 100M body limit, but a 5M symbolicate body limit
    let symbolicate_route = post(symbolicate).layer(DefaultBodyLimit::max(5 * 1024 * 1024));
    let symbolicate_any_route = post(symbolicate_any).layer(DefaultBodyLimit::max(5 * 1024 * 1024));
    Router::new()
        .route("/proxy/{*path}", get(proxy).head(proxy))
        .route("/requests/{request_id}", get(requests))
        .route("/applecrashreport", post(applecrashreport))
        .route("/minidump", post(minidump))
        .route("/symbolicate-js", post(symbolicate_js))
        .route("/symbolicate", symbolicate_route)
        .route("/symbolicate-any", symbolicate_any_route)
        .route("/symbolicate-jvm", post(symbolicate_jvm))
        .with_state(service)
        .layer(layer)
        // the healthcheck is last, as it will bypass all the middlewares
        .route("/healthcheck", get(healthcheck))
}
