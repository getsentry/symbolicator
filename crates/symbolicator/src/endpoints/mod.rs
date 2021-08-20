use actix_web::http::StatusCode;
use actix_web::{App, HttpResponse, ResponseError};

use crate::services::symbolication::MaxRequestsError;
use crate::services::Service;

mod applecrashreport;
mod healthcheck;
mod minidump;
mod proxy;
mod requests;
mod symbolicate;

/// Adds all endpoint routes to the app.
pub fn configure(app: App<Service>) -> App<Service> {
    app.configure(applecrashreport::configure)
        .configure(healthcheck::configure)
        .configure(minidump::configure)
        .configure(proxy::configure)
        .configure(requests::configure)
        .configure(symbolicate::configure)
}

impl ResponseError for MaxRequestsError {
    fn error_response(&self) -> actix_web::HttpResponse {
        HttpResponse::new(StatusCode::SERVICE_UNAVAILABLE)
    }
}
