use std::net::SocketAddr;
use std::net::TcpListener;

use anyhow::{Context, Result};

use futures::future::BoxFuture;
use futures::future::try_join_all;
use symbolicator_service::caching::Caches;

use crate::config::Config;
use crate::endpoints;
use crate::metric;
use crate::service::RequestService;

/// Starts all actors and HTTP (and optionally HTTPS) server based on loaded config.
pub fn run(config: Config) -> Result<()> {
    // Log this metric before actually starting the server. This allows to see restarts even if
    // service creation fails.
    metric!(counter("server.starting") += 1);

    let megs = 1024 * 1024;
    let io_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("sym-io")
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()?;
    let cpu_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("sym-cpu")
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()?;
    let web_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("sym-web")
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()?;

    let mut servers: Vec<BoxFuture<_>> = vec![];

    let service = RequestService::create(
        config.clone(),
        io_pool.handle().to_owned(),
        cpu_pool.handle().to_owned(),
    )
    .context("failed to create service state")?;

    let svc = endpoints::create_app(service).into_make_service();

    let socket_http = TcpListener::bind(config.bind.parse::<SocketAddr>()?)?;
    let local_addr = socket_http.local_addr()?;
    tracing::info!("Starting HTTP server on {}", local_addr);

    let server_http = axum_server::from_tcp(socket_http).serve(svc.clone());
    servers.push(Box::pin(server_http));

    if config.enable_cache_cleanup {
        let caches =
            Caches::from_config(&config).context("Failed to initialize cache configurations")?;
        tracing::info!("Starting cache cleanup thread");
        std::thread::spawn(move || caches.cleanup(false, Some(config.cache_cleanup_interval)));
    };

    web_pool.block_on(try_join_all(servers))?;
    tracing::info!("System shutdown complete");

    Ok(())
}
