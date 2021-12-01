use std::net::SocketAddr;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::endpoints;
use crate::services::Service;

/// Starts all actors and HTTP server based on loaded config.
pub fn run(config: Config) -> Result<()> {
    // Log this metric before actually starting the server. This allows to see restarts even if
    // service creation fails.
    metric!(counter("server.starting") += 1);

    let megs = 1024 * 1024;
    let io_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("symbolicator-io")
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()
        .unwrap();
    let cpu_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("symbolicator-cpu")
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()
        .unwrap();

    let socket = config.bind.parse::<SocketAddr>()?;

    let service = io_pool
        .block_on(Service::create(
            config,
            io_pool.handle().to_owned(),
            cpu_pool.handle().to_owned(),
        ))
        .context("failed to create service state")?;

    let _guard = io_pool.enter();
    let server =
        axum::Server::bind(&socket).serve(endpoints::create_app(service).into_make_service());

    log::info!("Starting server on {}", server.local_addr());

    io_pool.block_on(server)?;

    log::info!("System shutdown complete");

    Ok(())
}
