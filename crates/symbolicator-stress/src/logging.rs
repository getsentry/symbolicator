use std::collections::BTreeMap;
use std::env;
use std::future::Future;
use std::io::Write;
use std::net::{SocketAddr, TcpListener};
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
    pub sentry_server: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    // TODO: return the ports / futures of the http / upd servers to use
}

pub fn init(config: Config) -> Guard {
    if config.backtraces {
        env::set_var("RUST_BACKTRACE", "1");
    }

    let mut guard = Guard::default();

    if config.sentry {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(addr).unwrap();
        let socket = listener.local_addr().unwrap();

        guard.sentry_server = Some(Box::pin(async move {
            async fn ok() -> &'static str {
                "OK"
            }
            use axum::handler::HandlerWithoutStateExt;
            let server = axum::Server::from_tcp(listener)
                .unwrap()
                .serve(ok.into_make_service());
            server.await.unwrap()
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
        // we will log DEBUG output
        let rust_log = "INFO,symbolicator=DEBUG";
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
        let host = "TODO"; // create a *real* noop udp server to send metrics to

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