use axum::routing::{get, post};
use axum::Router;
use sentry_tower::{NewSentryLayer, SentryHttpLayer};
use tower::ServiceBuilder;

use crate::metrics::MetricsLayer;
use crate::services::Service;

mod applecrashreport;
mod error;
mod minidump;
mod multipart;
mod proxy;
mod requests;
mod symbolicate;

pub use error::ResponseError;

use self::minidump::handle_minidump_request as minidump;
use applecrashreport::handle_apple_crash_report_request as applecrashreport;
use proxy::proxy_symstore_request as proxy;
use requests::poll_request as requests;
use symbolicate::symbolicate_frames as symbolicate;

pub async fn healthcheck() -> &'static str {
    metric!(counter("healthcheck") += 1);
    "ok"
}

pub fn create_app(service: Service) -> Router {
    // The layers here go "top to bottom" according to the reading order here.
    let layer = ServiceBuilder::new()
        .layer(axum::AddExtensionLayer::new(service))
        .layer(NewSentryLayer::new_from_top())
        .layer(SentryHttpLayer::with_transaction())
        .layer(MetricsLayer);
    Router::new()
        .route("/proxy/:path", get(proxy).head(proxy))
        .route("/requests/:request_id", get(requests))
        .route("/symbolicate", post(symbolicate))
        .route("/minidump", post(minidump))
        .route("/applecrashreport", post(applecrashreport))
        .layer(layer)
        // the healthcheck is last, as it will bypass all the middlewares
        .route("/healthcheck", get(healthcheck))
}
