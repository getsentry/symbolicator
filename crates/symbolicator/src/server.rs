use actix_web::{server::HttpServer, App};
use anyhow::{Context, Result};

use crate::config::Config;
use crate::endpoints;
use crate::middlewares;
use crate::services::Service;
use crate::utils::sentry::SentryMiddleware;

/// Creates the Actix web application with all middlewares.
#[inline]
pub fn create_app(state: Service) -> App<Service> {
    App::with_state(state)
        .middleware(middlewares::Metrics)
        .middleware(middlewares::ErrorHandlers)
        .middleware(SentryMiddleware::new())
        .configure(endpoints::configure)
}

/// Starts all actors and HTTP server based on loaded config.
#[tracing::instrument(skip(config))]
pub fn run(config: Config) -> Result<()> {
    // Log this metric before actually starting the server. This allows to see restarts even if
    // service creation fails. The HTTP server is bound before the actix system runs.
    metric!(counter("server.starting") += 1);

    let io_pool = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("symbolicator-io")
        .enable_all()
        .build()
        .unwrap();
    let cpu_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("symbolicator-cpu")
        .enable_all()
        .build()
        .unwrap();

    let bind = config.bind.clone();

    let service = Service::create(
        config,
        io_pool.handle().to_owned(),
        cpu_pool.handle().to_owned(),
    )
    .context("failed to create service state")?;

    tracing::info!("Starting http server: {}", bind);
    HttpServer::new(move || create_app(service.clone()))
        .bind(&bind)
        .context("failed to bind to the port")?
        .run();
    tracing::info!("System shutdown complete");

    Ok(())
}
