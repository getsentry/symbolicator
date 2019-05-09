use std::env;
use std::fmt;
use std::io::{self, Write};

use chrono::{DateTime, Utc};
use failure::AsFail;
use log::{Level, LevelFilter};
use sentry::integrations::log as sentry_log;
use serde::{Deserialize, Serialize};

use crate::config::{Config, LogFormat};

fn get_rust_log(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::Off => "",
        LevelFilter::Error => "ERROR",
        LevelFilter::Warn => "WARN",
        LevelFilter::Info => {
            "INFO,\
             trust_dns_proto=WARN"
        }
        LevelFilter::Debug => {
            "INFO,\
             trust_dns_proto=WARN,\
             actix_web::pipeline=DEBUG,\
             symbolicator=DEBUG"
        }
        LevelFilter::Trace => {
            "INFO,\
             trust_dns_proto=WARN,\
             actix_web::pipeline=DEBUG,\
             symbolicator=TRACE"
        }
    }
}

fn pretty_logger() -> env_logger::Builder {
    pretty_env_logger::formatted_builder()
}

fn simplified_logger() -> env_logger::Builder {
    let mut builder = env_logger::Builder::new();
    builder.format(|buf, record| {
        writeln!(
            buf,
            "{} [{}] {}: {}",
            buf.timestamp(),
            record.module_path().unwrap_or("<unknown>"),
            record.level(),
            record.args()
        )
    });
    builder
}

#[derive(Serialize, Deserialize, Debug)]
struct LogRecord<'a> {
    timestamp: DateTime<Utc>,
    level: Level,
    logger: &'a str,
    message: String,
    module_path: Option<&'a str>,
    filename: Option<&'a str>,
    lineno: Option<u32>,
}

fn json_logger() -> env_logger::Builder {
    let mut builder = env_logger::Builder::new();
    builder.format(|mut buf, record| -> io::Result<()> {
        let record = LogRecord {
            timestamp: Utc::now(),
            level: record.level(),
            logger: record.target(),
            message: record.args().to_string(),
            module_path: record.module_path(),
            filename: record.file(),
            lineno: record.line(),
        };

        serde_json::to_writer(&mut buf, &record)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;

        buf.write_all(b"\n")?;
        Ok(())
    });
    builder
}

/// Initializes logging for the symbolicator.
///
/// This considers the `RUST_LOG` environment variable and defaults it to the level specified in the
/// configuration. Additionally, this toggles `RUST_BACKTRACE` based on the `enable_stacktraces`
/// config value.
pub fn init_logging(config: &Config) {
    if config.logging.enable_backtraces {
        env::set_var("RUST_BACKTRACE", "1");
    }

    if env::var("RUST_LOG").is_err() {
        let rust_log = get_rust_log(config.logging.level);
        env::set_var("RUST_LOG", rust_log);
    }

    let mut builder = match (config.logging.format, console::user_attended()) {
        (LogFormat::Auto, true) | (LogFormat::Pretty, _) => pretty_logger(),
        (LogFormat::Auto, false) | (LogFormat::Simplified, _) => simplified_logger(),
        (LogFormat::Json, _) => json_logger(),
    };

    match env::var("RUST_LOG") {
        Ok(rust_log) => builder.parse_filters(&rust_log),
        Err(_) => builder.filter_level(config.logging.level),
    };

    let logger = Box::new(builder.build());
    let global_filter = logger.filter();

    sentry_log::init(
        Some(logger),
        sentry_log::LoggerOptions {
            global_filter: Some(global_filter),
            ..Default::default()
        },
    );
}

/// Returns whether backtrace printing is enabled.
pub fn backtrace_enabled() -> bool {
    match std::env::var("RUST_BACKTRACE").as_ref().map(String::as_str) {
        Ok("1") | Ok("full") => true,
        _ => false,
    }
}

/// A wrapper around a `Fail` that prints its causes.
pub struct LogError<'a, E: AsFail>(pub &'a E);

impl<'a, E: AsFail> fmt::Display for LogError<'a, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fail = self.0.as_fail();

        write!(f, "{}", fail)?;
        for cause in fail.iter_causes() {
            write!(f, "\n  caused by: {}", cause)?;
        }

        if backtrace_enabled() {
            if let Some(backtrace) = fail.backtrace() {
                write!(f, "\n\n{:?}", backtrace)?;
            }
        }

        Ok(())
    }
}

/// Logs an error to the configured logger or `stderr` if not yet configured.
pub fn ensure_log_error<E: failure::AsFail>(error: &E) {
    if log::log_enabled!(log::Level::Error) {
        log::error!("{}", LogError(error));
    } else {
        eprintln!("error: {}", LogError(error));
    }
}
