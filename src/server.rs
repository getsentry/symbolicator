use actix_rt::System;
use actix_service::NewService;
use actix_web::dev::{Body, ServiceRequest, ServiceResponse};
use actix_web::{App, Error, HttpServer};
use failure::{Fail, ResultExt};
use sentry::integrations::failure::capture_fail;

use crate::config::Config;
use crate::endpoints;
use crate::metrics;
use crate::middleware;
use crate::service::Service;

/// Variants of `ServerError`.
#[derive(Clone, Copy, Debug, Fail)]
pub enum ServerErrorKind {
    /// Failed to bind to a port.
    #[fail(display = "failed to bind to the port")]
    Bind,

    /// Failed to start the server.
    #[fail(display = "failed to start the server")]
    Run,
}

symbolic::common::derive_failure!(
    ServerError,
    ServerErrorKind,
    doc = "Error when starting the server."
);

impl actix_web::ResponseError for ServerError {}

/// Creates the Actix web application with all middlewares.
#[inline]
pub(crate) fn create_app(
    service: Service,
) -> App<
    impl NewService<
        Config = (),
        Request = ServiceRequest,
        Response = ServiceResponse<Body>,
        Error = Error,
        InitError = (),
    >,
    Body,
> {
    App::new()
        .data(service)
        .wrap(
            middleware::Sentry::new()
                .error_reporter(capture_fail::<ServerError>)
                .capture_server_errors(true)
                .emit_header(true),
        )
        .wrap(middleware::ErrorHandler)
        .wrap(middleware::RequestMetrics)
        .configure(endpoints::configure)
}

/// Starts all actors and HTTP server based on loaded config.
pub fn run(config: Config) -> Result<(), ServerError> {
    let sys = System::new("symbolicator");

    if let Some(ref statsd) = config.metrics.statsd {
        metrics::configure_statsd(&config.metrics.prefix, statsd);
    }
    metric!(counter("server.starting") += 1);

    let bind = config.bind.clone();
    let service = Service::create(config);

    HttpServer::new(move || create_app(service.clone()))
        .bind(&bind)
        .context(ServerErrorKind::Bind)?
        .start();

    log::info!("Started http server: {}", bind);

    sys.run().context(ServerErrorKind::Run)?;
    Ok(())
}
