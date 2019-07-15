use actix_web::web;

pub mod applecrashreport;
pub mod healthcheck;
pub mod minidump;
pub mod proxy;
pub mod requests;
pub mod symbolicate;

/// Adds all endpoint routes to the app.
pub fn configure(config: &mut web::ServiceConfig) {
    applecrashreport::configure(config);
    healthcheck::configure(config);
    minidump::configure(config);
    proxy::configure(config);
    requests::configure(config);
    symbolicate::configure(config);
}
