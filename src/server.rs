use actix::System;
use actix_web::{server::HttpServer, App};
use anyhow::{Context, Result};

use crate::app::{ServiceApp, ServiceState};
use crate::config::Config;
use crate::endpoints;
use crate::middlewares;

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
pub fn run(config: Config) -> Result<()> {
    let sys = System::new("symbolicator");

    // Log this metric before actually starting the server. This allows to see restarts even if
    // service creation fails. The HTTP server is bound before the actix system runs.
    metric!(counter("server.starting") += 1);

    let bind = config.bind.clone();
    let service = ServiceState::create(config).context("failed to create service state")?;

    log::info!("Starting http server: {}", bind);
    HttpServer::new(move || create_app(service.clone()))
        .bind(&bind)
        .context("failed to bind to the port")?
        .start();

    log::info!("Starting system");
    sys.run();
    log::info!("System shutdown complete");

    Ok(())
}
