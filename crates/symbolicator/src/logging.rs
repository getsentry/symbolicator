use std::env;
use std::fmt;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::Layer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use crate::config::{Config, LogFormat};

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

    if env::var("RUST_LOG").is_err() {
        let rust_log = get_rust_log(config.logging.level);
        env::set_var("RUST_LOG", rust_log);
    }

    let filter = EnvFilter::from_default_env();
    let sentry = sentry::integrations::tracing::layer();
    let subscriber = FmtSubscriber::new().with(filter).with(sentry);
    let format = Layer::new();
    match (config.logging.format, console::user_attended()) {
        (LogFormat::Auto, true) | (LogFormat::Pretty, _) => {
            tracing::subscriber::set_global_default(subscriber.with(format.pretty()))
        }
        (LogFormat::Auto, false) | (LogFormat::Simplified, _) => {
            tracing::subscriber::set_global_default(subscriber.with(format.compact()))
        }
        (LogFormat::Json, _) => {
            tracing::subscriber::set_global_default(subscriber.with(format.json()))
        }
    }
    .expect("setting global default subscriber");
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
