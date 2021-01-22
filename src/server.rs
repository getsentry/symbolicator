use actix_web::{server::HttpServer, App};
use anyhow::{Context, Result};

use crate::app::{ServiceApp, ServiceState};
use crate::config::Config;
use crate::endpoints;
use crate::middlewares;
use crate::utils::sentry::SentryMiddleware;

/// Creates the Actix web application with all middlewares.
#[inline]
pub fn create_app(state: ServiceState) -> ServiceApp {
    App::with_state(state)
        .middleware(middlewares::Metrics)
        .middleware(middlewares::ErrorHandlers)
        .middleware(SentryMiddleware::new())
        .configure(endpoints::configure)
}

/// Starts all actors and HTTP server based on loaded config.
pub fn run(config: Config) -> Result<()> {
    // Log this metric before actually starting the server. This allows to see restarts even if
    // service creation fails. The HTTP server is bound before the actix system runs.
    metric!(counter("server.starting") += 1);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("symbolicator")
        .enable_all()
        .build()
        .unwrap();

    let bind = config.bind.clone();

    // Enter the tokio runtime before creating the services.
    let _guard = runtime.enter();
    let service = ServiceState::create(config).context("failed to create service state")?;

    log::info!("Starting http server: {}", bind);
    HttpServer::new(move || create_app(service.clone()))
        .bind(&bind)
        .context("failed to bind to the port")?
        .run();
    log::info!("System shutdown complete");

    Ok(())
}
