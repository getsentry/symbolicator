use actix_web::App;

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
