use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use failure::Fail;
use serde::Deserialize;

#[derive(Debug, Fail, derive_more::From)]
pub enum ConfigError {
    #[fail(display = "Failed to open file: {}", _0)]
    Io(#[fail(cause)] io::Error),

    #[fail(display = "Failed to parse YAML: {}", _0)]
    Parsing(#[fail(cause)] serde_yaml::Error),
}

/// Control the metrics.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Metrics {
    /// host/port of statsd instance
    pub statsd: Option<String>,

    /// The prefix that should be added to all metrics.
    pub prefix: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Which directory to use when caching. Default is not to cache.
    pub cache_dir: Option<PathBuf>,

    /// Which port to bind for, for HTTP interface
    pub bind: String,

    /// If set, configuration for reporting metrics to a statsd instance
    pub metrics: Metrics,

    /// DSN to report internal errors to
    pub sentry_dsn: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cache_dir: None,
            bind: "127.0.0.1:3021".to_owned(),
            metrics: Metrics::default(),
            sentry_dsn: None,
        }
    }
}

impl Config {
    pub fn get(path: Option<&Path>) -> Result<Self, ConfigError> {
        Ok(match path {
            Some(path) => serde_yaml::from_reader(File::open(path)?)?,
            None => Config::default(),
        })
    }
}
