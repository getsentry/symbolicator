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
    let middleware = ServiceBuilder::new()
        .layer(NewSentryLayer::new_from_top())
        .layer(SentryHttpLayer::new().enable_transaction())
        .layer(MetricsLayer);

    // Routes for symbolication requests that don't include large files. These can use a lower
    // body size limit.
    let symbolicate_router = Router::new()
        .route("/symbolicate-any", post(symbolicate_any))
        .route("/symbolicate-js", post(symbolicate_js))
        .route("/symbolicate-jvm", post(symbolicate_jvm))
        .route("/symbolicate", post(symbolicate))
        .layer(DefaultBodyLimit::max(
            service.config().symbolicate_body_max_bytes,
        ));

    // Routes for symbolication requests that may include large files. These need a higher
    // body size limit.
    let large_file_router = Router::new()
        .route("/applecrashreport", post(applecrashreport))
        .route("/minidump", post(minidump))
        .layer(DefaultBodyLimit::max(
            service.config().crash_file_body_max_bytes,
        ));

    // Miscellaneous routes that only accept `GET` requests. These can use `axum`'s default limit.
    let misc_router = Router::new()
        .route("/proxy/{*path}", get(proxy).head(proxy))
        .route("/requests/{request_id}", get(requests));

    Router::new()
        .merge(symbolicate_router)
        .merge(large_file_router)
        .merge(misc_router)
        .with_state(service)
        .layer(middleware)
        // the healthcheck is last, as it will bypass all the middlewares
        .route("/healthcheck", get(healthcheck))
}
