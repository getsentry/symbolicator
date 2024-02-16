use std::collections::BTreeMap;
use std::env;
use std::future::Future;
use std::io::Write;
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::pin::Pin;

use symbolicator_service::metrics;
use tracing_subscriber::fmt::fmt;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::prelude::*;

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

pub fn init(config: Config) -> Guard {
    if config.backtraces {
        env::set_var("RUST_BACKTRACE", "1");
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
                ..Default::default()
            },
        )));
    }

    if config.tracing {
        let rust_log = "INFO";
        let subscriber = fmt()
            .with_timer(UtcTime::rfc_3339())
            .with_target(true)
            .with_env_filter(rust_log);

        // we want all the tracing machinery to be active, but not spam the console,
        // so redirect everything into the void:
        let subscriber = subscriber.with_writer(|| NoopWriter);

        // this should mimic the settings used in production:
        subscriber
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .finish()
            .with(sentry::integrations::tracing::layer())
            .init();
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

        let host = format!("127.0.0.1:{}", socket.port());

        // have some default tags, just to be closer to the real world config
        let mut tags = BTreeMap::new();
        tags.insert("host".into(), "stresstest".into());
        tags.insert("env".into(), "stresstest".into());

        metrics::configure_statsd("symbolicator", host, tags);
    }

    guard
}

struct NoopWriter;
impl Write for NoopWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // try to prevent the compiler from optimizing away all the formatting code:
        let buf = std::hint::black_box(buf);

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
