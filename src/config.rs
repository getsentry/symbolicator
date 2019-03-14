use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

use failure::Fail;

use serde_yaml;

#[derive(Fail, Debug, derive_more::From)]
pub enum ConfigError {
    #[fail(display = "Failed to open file: {}", _0)]
    Io(#[fail(cause)] io::Error),

    #[fail(display = "Failed to parse YAML: {}", _0)]
    Parsing(#[fail(cause)] serde_yaml::Error),
}

#[derive(serde::Deserialize)]
pub struct Config {
    /// Which directory to use when caching. Default is not to cache.
    pub cache_dir: Option<PathBuf>,
    /// Which port to bind for, for HTTP interface
    #[serde(default = "default_bind")]
    pub bind: String,
    /// If set, configuration for reporting metrics to a statsd instance
    pub metrics: Option<Metrics>,
}

fn default_bind() -> String {
    "127.0.0.1:42069".to_owned()
}

/// Control the metrics.
#[derive(serde::Deserialize, Debug)]
pub struct Metrics {
    /// host/port of statsd instance
    pub statsd: String,
    /// The prefix that should be added to all metrics.
    pub prefix: String,
}

pub fn get_config(path: Option<PathBuf>) -> Result<Config, ConfigError> {
    let path_ref: &Path = path
        .as_ref()
        .map(|x| x.as_path())
        .unwrap_or_else(|| "config".as_ref());
    let file = File::open(path_ref)?;
    Ok(serde_yaml::from_reader(file)?)
}
