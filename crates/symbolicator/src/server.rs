#[cfg(feature = "https")]
use std::fs::read;
use std::net::SocketAddr;
use std::net::TcpListener;
#[cfg(feature = "https")]
use std::path::PathBuf;

use anyhow::{Context, Result};

#[cfg(feature = "https")]
use axum_server::tls_rustls::RustlsConfig;
use futures::future::BoxFuture;
use futures::future::try_join_all;

use crate::config::Config;
use crate::endpoints;
use crate::metric;
use crate::service::{CleanerService, RequestService};

#[cfg(feature = "https")]
fn read_pem_file(path: &PathBuf) -> Result<Vec<u8>> {
    read(path).context(format!("unable to read file: {}", path.display()))
}

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
    let background_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("sym-background")
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

    // The cleaner service runs in the background and does not block the main thread.
    // It will periodically clean up the cache based on the configuration.
    if let Some(cleaner) =
        CleanerService::create(config.clone(), background_pool.handle().to_owned())
    {
        tracing::info!("starting cache cleaner service");
        cleaner.start();
    } else {
        tracing::warn!("no cleaner service configured, cache cleanup will not run");
    }

    let svc = endpoints::create_app(service).into_make_service();

    let socket_http = TcpListener::bind(config.bind.parse::<SocketAddr>()?)?;
    let local_addr = socket_http.local_addr()?;
    tracing::info!("Starting HTTP server on {}", local_addr);

    #[allow(clippy::redundant_clone)] // we need `svc` for the https case below
    let server_http = axum_server::from_tcp(socket_http).serve(svc.clone());
    servers.push(Box::pin(server_http));

    #[cfg(feature = "https")]
    if let Some(ref bind_str) = config.bind_https {
        let https_conf = match config.server_config.https {
            None => panic!("Need HTTPS config"),
            Some(ref conf) => conf,
        };
        let socket_https = TcpListener::bind(bind_str.parse::<SocketAddr>()?)?;
        let local_addr = socket_https.local_addr()?;
        tracing::info!("Starting HTTPS server on {}", local_addr);

        let certificate = read_pem_file(&https_conf.certificate_path)?;
        let key = read_pem_file(&https_conf.key_path)?;
        let tls_config =
            web_pool.block_on(async { RustlsConfig::from_pem(certificate, key).await })?;

        let server_https = axum_server::from_tcp_rustls(socket_https, tls_config).serve(svc);
        servers.push(Box::pin(server_https));
    }

    web_pool.block_on(try_join_all(servers))?;
    tracing::info!("System shutdown complete");

    Ok(())
}
