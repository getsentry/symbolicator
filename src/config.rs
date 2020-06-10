use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use failure::Fail;
use log::LevelFilter;
use sentry::internals::Dsn;
use serde::Deserialize;

use crate::types::SourceConfig;

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

/// Options for fine-tuning cache expiry.
#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq)]
pub struct CacheConfig {
    /// Maximum duration since last use of cache item (item last used).
    #[serde(with = "humantime_serde", default)]
    pub max_unused_for: Option<Duration>,

    /// Maximum duration since creation of negative cache item (item age).
    #[serde(with = "humantime_serde", default)]
    pub retry_misses_after: Option<Duration>,

    /// Maximum duration since creation of malformed cache item (item age).
    #[serde(with = "humantime_serde", default)]
    pub retry_malformed_after: Option<Duration>,
}

impl CacheConfig {
    pub fn default_derived() -> Self {
        CacheConfig {
            max_unused_for: Some(Duration::from_secs(3600 * 24 * 7)),
            retry_misses_after: Some(Duration::from_secs(3600)),
            retry_malformed_after: Some(Duration::from_secs(3600 * 24)),
        }
    }

    pub fn default_downloaded() -> Self {
        CacheConfig {
            max_unused_for: Some(Duration::from_secs(3600 * 24)),
            retry_misses_after: Some(Duration::from_secs(3600)),
            retry_malformed_after: Some(Duration::from_secs(3600 * 24)),
        }
    }

    pub fn default_diagnostics() -> Self {
        CacheConfig {
            max_unused_for: Some(Duration::from_secs(3600 * 24)),
            retry_misses_after: None,
            retry_malformed_after: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct CacheConfigs {
    /// Configure how long downloads are cached for.
    pub downloaded: CacheConfig,
    /// Configure how long caches derived from downloads are cached for.
    pub derived: CacheConfig,
    /// How long diagnostics data is cached.
    ///
    /// E.g. minidumps which caused a crash in symbolicator will be stored here.
    ///
    /// Only `max_unused_for` is used here, the others do not make sense.
    pub diagnostics: CacheConfig,
}

impl Default for CacheConfigs {
    fn default() -> CacheConfigs {
        CacheConfigs {
            downloaded: CacheConfig::default_downloaded(),
            derived: CacheConfig::default_derived(),
            diagnostics: CacheConfig::default_diagnostics(),
        }
    }
}

/// See README.md for more information on config values.
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

    /// Fine-tune cache expiry
    pub caches: CacheConfigs,

    /// Enables symbol proxy mode.
    pub symstore_proxy: bool,

    /// Default list of sources and the sources used for proxy mode.
    pub sources: Arc<Vec<SourceConfig>>,

    /// Allow reserved IP addresses for requests to sources.
    pub connect_to_reserved_ips: bool,
}

impl Config {
    pub fn cache_dir<P>(&self, dir: P) -> Option<PathBuf>
    where
        P: AsRef<Path>,
    {
        self.cache_dir.as_ref().map(|base| base.join(dir))
    }

    pub fn default_sources(&self) -> Arc<Vec<SourceConfig>> {
        self.sources.clone()
    }
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

/// Default value for the "cache_dir" configuration.
fn default_cache_dir() -> Option<PathBuf> {
    if is_docker() {
        // Docker image already defines `/data` as a persistent volume
        Some(PathBuf::from("/data"))
    } else {
        None
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cache_dir: default_cache_dir(),
            bind: default_bind(),
            logging: Logging::default(),
            metrics: Metrics::default(),
            sentry_dsn: None,
            caches: CacheConfigs::default(),
            symstore_proxy: true,
            sources: Arc::new(vec![]),
            connect_to_reserved_ips: false,
        }
    }
}

impl Config {
    pub fn get(path: Option<&Path>) -> Result<Self, ConfigError> {
        match path {
            Some(path) => Self::from_reader(fs::File::open(path)?),
            None => Ok(Config::default()),
        }
    }

    fn from_reader(reader: impl std::io::Read) -> Result<Self, ConfigError> {
        let mut cfg: Config = serde_yaml::from_reader(reader)?;

        // The CacheConfig structs have different defaults depending on which cache they are
        // used for.  Since all values are Options for serde the defaults are None.  Here we
        // merge the defaults.  For `diagnostics` no patching is needed.
        let default_downloaded = CacheConfig::default_downloaded();
        if cfg.caches.downloaded.max_unused_for == None {
            cfg.caches.downloaded.max_unused_for = default_downloaded.max_unused_for;
        }
        if cfg.caches.downloaded.retry_misses_after == None {
            cfg.caches.downloaded.retry_misses_after = default_downloaded.retry_misses_after;
        }
        if cfg.caches.downloaded.retry_malformed_after == None {
            cfg.caches.downloaded.retry_malformed_after = default_downloaded.retry_malformed_after;
        }
        let default_derived = CacheConfig::default_derived();
        if cfg.caches.derived.max_unused_for == None {
            cfg.caches.derived.max_unused_for = default_derived.max_unused_for;
        }
        if cfg.caches.derived.retry_misses_after == None {
            cfg.caches.derived.retry_misses_after = default_derived.retry_misses_after;
        }
        if cfg.caches.derived.retry_malformed_after == None {
            cfg.caches.derived.retry_malformed_after = default_derived.retry_malformed_after;
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_config() {
        let cfg = Config::get(None).unwrap();
        assert_eq!(
            cfg.caches.diagnostics.max_unused_for,
            Some(Duration::from_secs(3600 * 24))
        );

        let yml = r#"
            caches:
              diagnostics:
                max_unused_for: 1h
        "#;
        let cfg = Config::from_reader(yml.as_bytes()).unwrap();
        assert_eq!(
            cfg.caches.diagnostics.max_unused_for,
            Some(Duration::from_secs(3600))
        );
        assert_eq!(cfg.caches.diagnostics.retry_misses_after, None);
        assert_eq!(cfg.caches.diagnostics.retry_malformed_after, None);

        assert_eq!(cfg.caches.downloaded, CacheConfig::default_downloaded());
        assert_eq!(cfg.caches.derived, CacheConfig::default_derived());

        let yml = r#"
            caches:
              downloaded:
                max_unused_for: 500s
        "#;
        let cfg = Config::from_reader(yml.as_bytes()).unwrap();
        assert_eq!(
            cfg.caches.downloaded.max_unused_for,
            Some(Duration::from_secs(500))
        );
        assert_eq!(
            cfg.caches.downloaded.retry_misses_after,
            Some(Duration::from_secs(3600))
        );
        assert_eq!(
            cfg.caches.downloaded.retry_malformed_after,
            Some(Duration::from_secs(3600 * 24))
        );
        assert_eq!(cfg.caches.derived, CacheConfig::default_derived());
        assert_eq!(cfg.caches.diagnostics, CacheConfig::default_diagnostics());
    }
}
