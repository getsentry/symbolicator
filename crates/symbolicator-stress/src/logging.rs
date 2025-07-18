use std::collections::BTreeMap;
use std::env;
use std::future::Future;
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::pin::Pin;

use symbolicator_service::{logging, metrics};

#[derive(Debug, Default)]
pub struct Config {
    pub backtraces: bool,
    pub sentry: bool,
    pub tracing: bool,
    pub metrics: bool,
}

#[derive(Default)]
pub struct Guard {
    sentry: Option<sentry::ClientInitGuard>,
    pub http_sink: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    pub udp_sink: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

/// Initializes logging according to the config.
///
/// # Safety
/// This function uses [`std::env::set_var`] to modify the environment. That function is only safe
/// to call in single-threaded contexts to prevent unsynchronized concurrent access to the environment.
pub unsafe fn init(config: Config) -> Guard {
    if config.backtraces {
        // SAFETY: As documented, this function may only be called in a single-threaded context.
        unsafe { env::set_var("RUST_BACKTRACE", "1") };
    }

    let mut guard = Guard::default();

    if config.sentry {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let socket = listener.local_addr().unwrap();

        guard.http_sink = Some(Box::pin(async move {
            async fn ok() -> &'static str {
                "OK"
            }
            use axum::handler::HandlerWithoutStateExt;

            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, ok.into_make_service()).await.unwrap();
        }));

        let dsn = format!("http://some_token@127.0.0.1:{}/1234", socket.port());

        guard.sentry = Some(sentry::init((
            dsn,
            sentry::ClientOptions {
                release: sentry::release_name!(),
                traces_sample_rate: 1.0,
                enable_logs: true,
                ..Default::default()
            },
        )));
    }

    if config.tracing {
        let env_filter = "INFO,\
             minidump=ERROR,\
             trust_dns_proto=WARN";
        // we want all the tracing machinery to be active and use the production JSON output,
        // but not spam the console, so redirect everything into the void (`std::io::sink`):
        logging::init_json_logging(env_filter, std::io::sink);
    }

    if config.metrics {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = UdpSocket::bind(addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let socket = listener.local_addr().unwrap();

        guard.udp_sink = Some(Box::pin(async move {
            let listener = tokio::net::UdpSocket::from_std(listener).unwrap();
            let mut buf = Vec::with_capacity(1024);
            loop {
                buf.clear();
                let _len = listener.recv_buf(&mut buf).await.unwrap();
            }
        }));

        let addrs = vec![([127, 0, 0, 1], socket.port()).into()];

        // have some default tags, just to be closer to the real world config
        let mut tags = BTreeMap::new();
        tags.insert("host".into(), "stresstest".into());
        tags.insert("env".into(), "stresstest".into());

        metrics::configure_statsd("symbolicator", addrs, tags);
    }

    guard
}
