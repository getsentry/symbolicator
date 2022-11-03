use axum_server::Handle;
#[cfg(feature = "https")]
use std::fs::read;
use std::net::SocketAddr;
#[cfg(feature = "https")]
use std::path::PathBuf;

use anyhow::{Context, Result};

#[cfg(feature = "https")]
use axum_server::tls_rustls::RustlsConfig;
use futures::future::try_join_all;
use futures::future::BoxFuture;

use crate::config::Config;
use crate::endpoints;
use crate::metric;
use crate::services::Service;

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
        .thread_name("symbolicator-io")
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()?;
    let cpu_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("symbolicator-cpu")
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()?;
    let web_pool = tokio::runtime::Builder::new_multi_thread()
        .thread_name("symbolicator-web")
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()?;

    let mut servers: Vec<BoxFuture<_>> = vec![];

    let service = web_pool
        .block_on(Service::create(
            config.clone(),
            io_pool.handle().to_owned(),
            cpu_pool.handle().to_owned(),
        ))
        .context("failed to create service state")?;

    let svc = endpoints::create_app(service).into_make_service();

    let handle_http = Handle::new();
    let socket_http = config.bind.parse::<SocketAddr>()?;
    let server_http = axum_server::bind(socket_http)
        .handle(handle_http.clone())
        .serve(svc.clone());
    servers.push(Box::pin(server_http));

    let listening_http = async move {
        if let Some(local_addr) = handle_http.listening().await {
            tracing::info!("Starting HTTP server on {}", local_addr);
        } else {
            panic!("Unable to listen on HTTP port");
        }
        Ok(())
    };
    servers.push(Box::pin(listening_http));

    #[cfg(feature = "https")]
    if let Some(ref bind_str) = config.bind_https {
        let handle_https = Handle::new();
        let https_conf = match config.server_config.https {
            None => panic!("Need HTTPS config"),
            Some(ref conf) => conf,
        };
        let socket_https = bind_str.parse::<SocketAddr>()?;
        let certificate = read_pem_file(&https_conf.certificate_path)?;
        let key = read_pem_file(&https_conf.key_path)?;
        let tls_config =
            web_pool.block_on(async { RustlsConfig::from_pem(certificate, key).await })?;
        let server_https = axum_server::bind_rustls(socket_https, tls_config)
            .handle(handle_https.clone())
            .serve(svc);
        servers.push(Box::pin(server_https));

        let listening_https = async move {
            if let Some(local_addr) = handle_https.listening().await {
                tracing::info!("Starting HTTPS server on {}", local_addr);
            } else {
                panic!("Unable to listen on HTTPS port");
            }
            Ok(())
        };
        servers.push(Box::pin(listening_https));
    }

    web_pool.block_on(try_join_all(servers))?;
    tracing::info!("System shutdown complete");

    Ok(())
}
