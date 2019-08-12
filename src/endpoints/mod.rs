use actix_web::web;

mod applecrashreport;
mod healthcheck;
mod minidump;
mod proxy;
mod requests;
mod symbolicate;

/// Adds all endpoint routes to the app.
pub fn configure(config: &mut web::ServiceConfig) {
    applecrashreport::configure(config);
    healthcheck::configure(config);
    minidump::configure(config);
    proxy::configure(config);
    requests::configure(config);
    symbolicate::configure(config);
}
