use actix::System;
use actix_web::{server::HttpServer, App};
use failure::{Fail, ResultExt};
use num_cpus;

use crate::app::{ServiceApp, ServiceState};
use crate::config::Config;
use crate::endpoints;
use crate::metrics;
use crate::middlewares;

/// Variants of `ServerError`.
#[derive(Clone, Copy, Debug, Fail)]
pub enum ServerErrorKind {
    /// Failed to bind to a port.
    #[fail(display = "failed to bind to the port")]
    Bind,
}

symbolic::common::derive_failure!(
    ServerError,
    ServerErrorKind,
    doc = "Error when starting the server."
);

impl actix_web::ResponseError for ServerError {}

/// Creates the Actix web application with all middlewares.
#[inline]
fn create_app(state: ServiceState) -> ServiceApp {
    App::with_state(state)
        .middleware(middlewares::Metrics)
        .middleware(middlewares::ErrorHandlers)
        .middleware(sentry_actix::SentryMiddleware::new())
        .configure(endpoints::configure)
}

/// Starts all actors and HTTP server based on loaded config.
pub fn run(config: Config) -> Result<(), ServerError> {
    let sys = System::new("symbolicator");

    if let Some(ref statsd) = config.metrics.statsd {
        metrics::configure_statsd(&config.metrics.prefix, statsd);
    }

    // Log this metric before actually starting the server. This allows to see restarts even if
    // service creation fails. The HTTP server is bound before the actix system runs.
    metric!(counter("server.starting") += 1);

    let bind = config.bind.clone();
    let service = ServiceState::create(config);

    log::info!("Starting http server: {}", bind);
    HttpServer::new(move || create_app(service.clone()))
        .workers(num_cpus::get() / 2)
        .bind(&bind)
        .context(ServerErrorKind::Bind)?
        .start();

    log::info!("Starting system");
    sys.run();
    log::info!("System shutdown complete");

    Ok(())
}
