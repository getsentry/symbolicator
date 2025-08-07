use std::env;

use sentry::integrations::tracing::EventFilter;
use symbolicator_service::logging::init_json_logging;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
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
             breakpad_symbols=ERROR,\
             goblin=ERROR,\
             minidump=ERROR"
        }
        LevelFilter::INFO => {
            "INFO,\
             breakpad_symbols=ERROR,\
             goblin=ERROR,\
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
/// configuration. Additionally, this toggles `RUST_BACKTRACE` based on the
/// [`enable_backtraces`](crate::config::Logging::enable_backtraces)
/// config value.
///
/// # Safety
/// This function uses [`std::env::set_var`] to modify the environment. That function is only safe
/// to call in single-threaded contexts to prevent unsynchronized concurrent access to the environment.
pub unsafe fn init_logging(config: &Config) {
    if config.logging.enable_backtraces {
        // SAFETY: As documented, this function may only be called in a single-threaded context.
        unsafe { env::set_var("RUST_BACKTRACE", "1") };
    }

    let rust_log =
        env::var("RUST_LOG").unwrap_or_else(|_| get_rust_log(config.logging.level).to_string());

    let fmt_layer = {
        let layer = tracing_subscriber::fmt::layer()
            .with_timer(UtcTime::rfc_3339())
            .with_target(true);

        match (config.logging.format, console::user_attended()) {
            (LogFormat::Auto, true) | (LogFormat::Pretty, _) => layer.pretty().boxed(),
            (LogFormat::Auto, false) | (LogFormat::Simplified, _) => {
                layer.compact().with_ansi(false).boxed()
            }
            (LogFormat::Json, _) => {
                init_json_logging(&rust_log, std::io::stdout);
                return;
            }
        }
    }
    .with_filter(EnvFilter::new(&rust_log));

    // Same as the default filter, except it sends everything at or above INFO as logs instead of breadcrumbs.
    let sentry_layer =
        sentry::integrations::tracing::layer().event_filter(|md| match *md.level() {
            tracing::Level::ERROR => EventFilter::Event | EventFilter::Log,
            tracing::Level::WARN | tracing::Level::INFO => EventFilter::Log,
            tracing::Level::DEBUG | tracing::Level::TRACE => EventFilter::Ignore,
        });

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(sentry_layer)
        .init();
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
