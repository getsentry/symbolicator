use std::collections::BTreeMap;
use std::env;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::level_filters::LevelFilter;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::fmt::time::ChronoUtc;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use crate::config::{Config, LogFormat};

//#[derive(Serialize, Deserialize, Debug)]
//struct JsonLog<'a> {
//    timestamp: DateTime<Utc>,
//    level: Level,
//    logger: &'a str,
//    message: String,
//    module_path: Option<&'a str>,
//    filename: Option<&'a str>,
//    lineno: Option<u32>,
//    fields: BTreeMap<&'a str, &'a str>,
//}
//
//impl<'a, 'b> From<&'b Event<'a>> for JsonLog<'a> {
//    fn from(event: &'b Event<'a>) -> Self {
//        let meta = event.metadata();
//        Self {
//            timestamp: Utc::now(),
//            level: *meta.level(),
//            logger: meta.target(),
//            message: event.message(),
//            module_path: meta.module_path(),
//            filename: meta.file(),
//            lineno: meta.line(),
//        }
//    }
//}
//
//#[derive(Debug)]
//struct Json;
//
//impl<S, N> FormatEvent<S, N> for Json
//where
//    S: Subscriber + for<'a> LookupSpan<'a>,
//    N: for<'a> FormatFields<'a> + 'static,
//{
//    fn format_event(
//        &self,
//        ctx: &FmtContext<'_, S, N>,
//        writer: &mut dyn fmt::Write,
//        event: &Event<'_>,
//    ) -> fmt::Result {
//        let meta = event.metadata();
//        let module_path = meta.module_path();
//        let level = *meta.level();
//        let logger = meta.target();
//        let lineno = meta.line();
//        let timestamp = Utc::now();
//
//        writer.write
//    }
//}
fn get_rust_log(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::OFF => "",
        LevelFilter::ERROR => "ERROR",
        LevelFilter::WARN => "WARN",
        LevelFilter::INFO => {
            "INFO,\
             trust_dns_proto=WARN"
        }
        LevelFilter::DEBUG => {
            "INFO,\
             trust_dns_proto=WARN,\
             actix_web::pipeline=DEBUG,\
             symbolicator=DEBUG"
        }
        LevelFilter::TRACE => {
            "INFO,\
             trust_dns_proto=WARN,\
             actix_web::pipeline=DEBUG,\
             symbolicator=TRACE"
        }
    }
}

/// Initializes logging for the symbolicator.
///
/// This considers the `RUST_LOG` environment variable and defaults it to the level specified in the
/// configuration. Additionally, this toggles `RUST_BACKTRACE` based on the [`enable_stacktraces`]
/// config value.
///
/// [`enable_stacktraces`]: crate::config::Logging::enable_backtraces
pub fn init_logging(config: &Config) {
    if config.logging.enable_backtraces {
        env::set_var("RUST_BACKTRACE", "1");
    }

    let rust_log =
        env::var("RUST_LOG").unwrap_or_else(|_| get_rust_log(config.logging.level).to_string());

    let filter = EnvFilter::new(rust_log);
    let sentry = sentry::integrations::tracing::layer();
    let subscriber = FmtSubscriber::new().with(filter).with(sentry);

    let format = Layer::new().with_timer(ChronoUtc::rfc3339());
    match (config.logging.format, console::user_attended()) {
        (LogFormat::Auto, true) | (LogFormat::Pretty, _) => {
            tracing::subscriber::set_global_default(subscriber.with(format.pretty()))
        }
        (LogFormat::Auto, false) | (LogFormat::Simplified, _) => {
            tracing::subscriber::set_global_default(subscriber.with(format.compact()))
        }
        (LogFormat::Json, _) => tracing::subscriber::set_global_default(
            subscriber.with(
                format
                    .json()
                    .flatten_event(true)
                    .with_current_span(false)
                    .with_span_list(false),
            ),
        ),
    }
    .expect("setting global default subscriber");

    // "Logger" that captures log records and republishes them as tracing events.
    tracing_log::LogTracer::init().expect("setting global default log tracer");
}

/// A wrapper around an [`Error`](std::error::Error) that prints its causes.
pub struct LogError<'a>(pub &'a dyn std::error::Error);

impl<'a> fmt::Display for LogError<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut error = self.0;
        write!(f, "{}", error)?;
        while let Some(cause) = error.source() {
            write!(f, "\n  caused by: {}", cause)?;
            error = cause;
        }

        Ok(())
    }
}

/// Logs an error to the configured logger or `stderr` if not yet configured.
pub fn ensure_log_error(error: &anyhow::Error) {
    if tracing::Level::ERROR <= tracing::level_filters::STATIC_MAX_LEVEL
        && tracing::Level::ERROR <= LevelFilter::current()
    {
        tracing::error!("{:?}", error);
    } else {
        eprintln!("{:?}", error);
    }
}
