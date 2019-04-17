use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use failure::Fail;
use log::LevelFilter;
use sentry::internals::Dsn;
use serde::Deserialize;

#[derive(Debug, Fail, derive_more::From)]
pub enum ConfigError {
    #[fail(display = "Failed to open file")]
    Io(#[fail(cause)] io::Error),

    #[fail(display = "Failed to parse YAML")]
    Parsing(#[fail(cause)] serde_yaml::Error),
}

/// Controls the log format
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Auto detect (pretty for tty, simplified for other)
    Auto,
    /// With colors
    Pretty,
    /// Simplified log output
    Simplified,
    /// Dump out JSON lines
    Json,
}

/// Controls the logging system.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Logging {
    /// The log level for the relay.
    pub level: LevelFilter,
    /// Controls the log format.
    pub format: LogFormat,
    /// When set to true, backtraces are forced on.
    pub enable_backtraces: bool,
}

impl Default for Logging {
    fn default() -> Self {
        Logging {
            level: LevelFilter::Info,
            format: LogFormat::Auto,
            enable_backtraces: true,
        }
    }
}

/// Control the metrics.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Metrics {
    /// host/port of statsd instance
    pub statsd: Option<String>,
    /// The prefix that should be added to all metrics.
    pub prefix: String,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            statsd: None,
            prefix: "symbolicator".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Which directory to use when caching. Default is not to cache.
    pub cache_dir: Option<PathBuf>,

    /// Host and port to bind the HTTP webserver to.
    pub bind: String,

    /// Configuration for internal logging.
    pub logging: Logging,

    /// Configuration for reporting metrics to a statsd instance.
    pub metrics: Metrics,

    /// DSN to report internal errors to
    pub sentry_dsn: Option<Dsn>,
}

/// Checks if we are running in docker.
fn is_docker() -> bool {
    if fs::metadata("/.dockerenv").is_ok() {
        return true;
    }

    fs::read_to_string("/proc/self/cgroup")
        .map(|s| s.find("/docker").is_some())
        .unwrap_or(false)
}

/// Default value for the "bind" configuration.
fn default_bind() -> String {
    if is_docker() {
        // Docker images rely on this service being exposed
        "0.0.0.0:3021".to_owned()
    } else {
        "127.0.0.1:3021".to_owned()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cache_dir: None,
            bind: default_bind(),
            logging: Logging::default(),
            metrics: Metrics::default(),
            sentry_dsn: None,
        }
    }
}

impl Config {
    pub fn get(path: Option<&Path>) -> Result<Self, ConfigError> {
        Ok(match path {
            Some(path) => serde_yaml::from_reader(fs::File::open(path)?)?,
            None => Config::default(),
        })
    }
}
