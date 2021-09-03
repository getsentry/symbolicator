use axum::handler::{get, post};
use axum::Router;
use sentry::integrations::tower::NewSentryLayer;

use crate::metrics::MetricsLayer;
use crate::services::Service;
use crate::utils::sentry::SentryRequestLayer;

mod applecrashreport;
mod error;
mod minidump;
mod proxy;
mod requests;
mod symbolicate;

pub use error::ResponseError;

use applecrashreport::handle_apple_crash_report_request as applecrashreport;
use minidump::handle_minidump_request as minidump;
use proxy::proxy_symstore_request as proxy;
use requests::poll_request as requests;
use symbolicate::symbolicate_frames as symbolicate;

pub async fn healthcheck() -> &'static str {
    metric!(counter("healthcheck") += 1);
    "ok"
}

pub type App = Router<axum::routing::BoxRoute>;

pub fn create_app(service: Service) -> App {
    Router::new()
        .route("/proxy/:path", get(proxy).head(proxy))
        .route("/requests/:request_id", get(requests))
        .route("/symbolicate", post(symbolicate))
        .route("/minidump", post(minidump))
        .route("/applecrashreport", post(applecrashreport))
        .layer(axum::AddExtensionLayer::new(service))
        .layer(SentryRequestLayer)
        .layer(NewSentryLayer::new_from_top())
        .layer(MetricsLayer)
        // the healthcheck is last, as it will bypass all the middlewares
        .route("/healthcheck", get(healthcheck))
        .boxed()
}
