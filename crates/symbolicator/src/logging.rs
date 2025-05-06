use std::env;

use symbolicator_service::logging::init_json_logging;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::fmt;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{Config, LogFormat};

fn get_rust_log(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::OFF => "",
        LevelFilter::ERROR => "ERROR",
        LevelFilter::WARN => {
            "WARN,\
             minidump=ERROR"
        }
        LevelFilter::INFO => {
            "INFO,\
             minidump=ERROR,\
             trust_dns_proto=WARN"
        }
        LevelFilter::DEBUG => {
            "INFO,\
             trust_dns_proto=WARN,\
             symbolicator=DEBUG"
        }
        LevelFilter::TRACE => {
            "INFO,\
             trust_dns_proto=WARN,\
             symbolicator=TRACE"
        }
    }
}

/// Initializes logging for the symbolicator.
///
/// This considers the `RUST_LOG` environment variable and defaults it to the level specified in the
/// configuration.
///
///
pub fn init_logging(config: &Config) {
    if config.logging.enable_backtraces {
        tracing::warn!(
            "`enable_backtraces` is deprecated: run Symbolicator with \
            `RUST_BACKTRACE=1` to enable backtraces."
        );
    }

    let rust_log =
        env::var("RUST_LOG").unwrap_or_else(|_| get_rust_log(config.logging.level).to_string());

    let subscriber = fmt()
        .with_timer(UtcTime::rfc_3339())
        .with_target(true)
        .with_env_filter(&rust_log);

    match (config.logging.format, console::user_attended()) {
        (LogFormat::Auto, true) | (LogFormat::Pretty, _) => subscriber
            .pretty()
            .finish()
            .with(sentry::integrations::tracing::layer())
            .init(),
        (LogFormat::Auto, false) | (LogFormat::Simplified, _) => subscriber
            .compact()
            .with_ansi(false)
            .finish()
            .with(sentry::integrations::tracing::layer())
            .init(),
        (LogFormat::Json, _) => init_json_logging(&rust_log, std::io::stdout),
    }
}

/// Logs an error to the configured logger or `stderr` if not yet configured.
pub fn ensure_log_error(error: &anyhow::Error) {
    if tracing::Level::ERROR <= tracing::level_filters::STATIC_MAX_LEVEL
        && tracing::Level::ERROR <= LevelFilter::current()
    {
        tracing::error!("{:?}", error);
    } else {
        eprintln!("{error:?}");
    }
}
