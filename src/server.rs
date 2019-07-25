use actix_rt::System;
use actix_web::{App, HttpServer};
use failure::{Fail, ResultExt};
use sentry::integrations::failure::capture_fail;

use crate::config::Config;
use crate::endpoints;
use crate::metrics;
use crate::middleware;
use crate::service::Service;

/// Errors t
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

/// Starts all actors and HTTP server based on loaded config.
pub fn run(config: Config) -> Result<(), ServerError> {
    let sys = System::new("symbolicator");

    if let Some(ref statsd) = config.metrics.statsd {
        metrics::configure_statsd(&config.metrics.prefix, statsd);
    }

    let service = Service::create(config);

    HttpServer::new(clone!(service, || {
        App::new()
            .data(service.clone())
            .wrap(
                middleware::Sentry::new()
                    .error_reporter(capture_fail::<ServerError>)
                    .capture_server_errors(true)
                    .emit_header(true),
            )
            .wrap(middleware::RequestMetrics)
            .configure(endpoints::configure)
    }))
    .bind(&service.config().bind)
    .context(ServerErrorKind::Bind)?
    .start();

    log::info!("Started http server: {}", service.config().bind);

    sys.run().context(ServerErrorKind::Run)?;
    Ok(())
}
