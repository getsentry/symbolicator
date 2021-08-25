use std::env;
use std::fmt;
use std::io::{self, Write};

use chrono::{DateTime, Utc};
use sentry::integrations::tracing::{breadcrumb_from_event, event_from_event};
use serde::{Deserialize, Serialize};
use tracing::level_filters::LevelFilter;
use tracing::{span, Level, Subscriber};
use tracing_subscriber::fmt::{fmt, SubscriberBuilder};
use tracing_subscriber::EnvFilter;

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

/// A delegating logger that also logs breadcrumbs.
pub struct BreadcrumbLogger<L> {
    inner: L,
}

impl<L> BreadcrumbLogger<L> {
    /// Initializes a new breadcrumb logger.
    pub fn new(inner: L) -> Self {
        Self { inner }
    }
}

impl<L> Subscriber for BreadcrumbLogger<L>
where
    L: Subscriber,
{
    fn enabled(&self, md: &tracing::Metadata<'_>) -> bool {
        self.inner.enabled(md)
    }

    fn new_span(&self, span: &span::Attributes<'_>) -> span::Id {
        self.inner.new_span(span)
    }

    fn record(&self, span: &span::Id, values: &span::Record<'_>) {
        self.inner.record(span, values);
    }

    fn record_follows_from(&self, span: &span::Id, follows: &span::Id) {
        self.inner.record_follows_from(span, follows);
    }

    fn enter(&self, span: &span::Id) {
        self.inner.enter(span);
    }

    fn exit(&self, span: &span::Id) {
        self.inner.exit(span);
    }

    fn event(&self, event: &tracing::Event<'_>) {
        if self.enabled(event.metadata()) {
            if *event.metadata().level() == Level::ERROR {
                sentry::capture_event(event_from_event(event));
            }

            sentry::add_breadcrumb(|| breadcrumb_from_event(event));
            self.inner.event(event);
        }
    }
}

fn set_global_logger<L: Subscriber + Sync + Send>(logger: L) {
    tracing::subscriber::set_global_default(BreadcrumbLogger::new(logger))
        .expect("setting global default subscriber")
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

    let builder = fmt().with_env_filter(EnvFilter::from_default_env());
    match (config.logging.format, console::user_attended()) {
        (LogFormat::Auto, true) | (LogFormat::Pretty, _) => {
            set_global_logger(builder.pretty().finish())
        }
        (LogFormat::Auto, false) | (LogFormat::Simplified, _) => {
            set_global_logger(builder.compact().finish())
        }
        (LogFormat::Json, _) => set_global_logger(builder.json().finish()),
    };
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
