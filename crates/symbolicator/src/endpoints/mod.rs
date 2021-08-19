use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::handler::{get, post};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use sentry::integrations::tower::NewSentryLayer;
use tower_layer::Layer;
use tower_service::Service as TowerService;

mod applecrashreport;
mod minidump;
mod proxy;
mod requests;
mod symbolicate;

pub use applecrashreport::handle_apple_crash_report_request as applecrashreport;
pub use minidump::handle_minidump_request as minidump;
pub use proxy::proxy_symstore_request as proxy;
pub use requests::poll_request as requests;
pub use symbolicate::symbolicate_frames as symbolicate;

use crate::metrics::MetricsLayer;
use crate::services::symbolication::MaxRequestsError;
use crate::services::Service;

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
        .layer(NewSentryLayer::new_from_top())
        .layer(MetricsLayer)
        // the healthcheck is last, as it will bypass all the middlewares
        .route("/healthcheck", get(healthcheck))
        .boxed()
}

#[derive(Debug)]
pub enum ResponseError {
    Anyhow(anyhow::Error),
    WithStatus(StatusCode, String),
}

impl IntoResponse for ResponseError {
    type Body = Body;
    type BodyError = <Self::Body as axum::body::HttpBody>::Error;

    fn into_response(self) -> Response<Self::Body> {
        match self {
            ResponseError::Anyhow(e) => Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(e.to_string())),
            ResponseError::WithStatus(status, e) => {
                Response::builder().status(status).body(Body::from(e))
            }
        }
        .unwrap()
    }
}

impl<E> From<E> for ResponseError
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(e: E) -> Self {
        Self::Anyhow(anyhow::Error::from(e))
    }
}

fn map_max_requests_error(_: MaxRequestsError) -> ResponseError {
    ResponseError::WithStatus(
        StatusCode::SERVICE_UNAVAILABLE,
        "Maximum number of concurrent requests reached".to_owned(),
    )
}
