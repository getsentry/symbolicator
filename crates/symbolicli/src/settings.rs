use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use symbolicator_service::config::{CacheConfigs, Config};
use symbolicator_sources::SourceConfig;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use reqwest::Url;
use serde::Deserialize;
use tracing::level_filters::LevelFilter;

/// The default API URL
pub const DEFAULT_URL: &str = "https://sentry.io/";

/// The name of the configuration file.
pub const CONFIG_RC_FILE_NAME: &str = ".symboliclirc";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Outputs the entire symbolication result as JSON.
    Json,
    /// Outputs the crashed thread as a detailed list of frames.
    ///
    /// For a JavaScript event, this will also print a list of errors
    /// that occurred during symbolication.
    Pretty,
    /// Outputs the crashed thread as a table.
    Compact,
}

/// Controls whether `symbolicli` will attempt to access a Sentry server.
#[derive(Clone, Debug)]
pub enum Mode {
    Offline,
    Online {
        org: String,
        project: String,
        auth_token: String,
        base_url: reqwest::Url,
        scraping_enabled: bool,
    },
}

/// A utility that provides local symbolication of Sentry events.
///
/// A valid auth token needs to be provided via the `--auth-token` option,
/// the `SENTRY_AUTH_TOKEN` environment variable, or `~/.symboliclirc`.
///
/// The output format can be controlled with the `--format` option.
#[derive(Clone, Parser, Debug)]
#[command(author, version, about, long_about)]
struct Cli {
    /// The event to symbolicate.
    ///
    /// This can either be the name of a local file (minidump or event JSON)
    /// or an event ID.
    pub event: String,

    /// The organization slug.
    #[arg(long, short)]
    pub org: Option<String>,

    /// The project slug.
    #[arg(long, short)]
    pub project: Option<String>,

    /// The URL of the sentry instance to connect to.
    ///
    /// Defaults to `https://sentry.io/`.
    #[arg(long)]
    pub url: Option<String>,

    /// The Sentry auth token to use to access the event and DIFs.
    ///
    /// This can alternatively be passed via the `SENTRY_AUTH_TOKEN` environment variable
    /// or `~/.symboliclirc`.
    #[arg(long = "auth-token")]
    pub auth_token: Option<String>,

    /// The output format.
    #[arg(long, value_enum, default_value = "json")]
    format: OutputFormat,

    /// Run in offline mode, i.e., don't access the Sentry server.
    ///
    /// In offline mode symbolicli will still access manually configured symbol sources.
    /// JavaScript symbolication is currently not supported in offline mode.
    #[arg(long)]
    offline: bool,

    /// The severity level of logging output.
    ///
    /// Possible values:
    /// off, error, warn, info, debug, trace
    #[arg(long, value_enum, default_value = "info")]
    log_level: LevelFilter,

    /// Disallow scraping of JavaScript source and sourcemap files
    /// from the internet.
    ///
    /// This flag has no effect on native symbolication.
    #[arg(long)]
    no_scrape: bool,

    /// An additional directory containing native symbols.
    ///
    /// The symbols must conform to the `unified` symbol server
    /// layout, as produced by `symsorter`.
    #[arg(long)]
    symbols: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
struct ConfigFile {
    pub org: Option<String>,
    pub project: Option<String>,
    pub url: Option<String>,
    pub auth_token: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub sources: Vec<SourceConfig>,
}

impl ConfigFile {
    pub fn parse(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(buf) => toml::from_str(&buf).context("Could not parse configuration file"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "Configuration file not found");
                Ok(Self::default())
            }
            Err(e) => Err(e).context(format!(
                "Could not read configuration file at {}",
                path.display()
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub event_id: String,
    pub symbolicator_config: Config,
    pub output_format: OutputFormat,
    pub log_level: LevelFilter,
    pub mode: Mode,
    pub symbols: Option<PathBuf>,
}

impl Settings {
    pub fn get() -> Result<Self> {
        let cli = Cli::parse();

        let global_config_path = find_global_config_file()?;
        let mut global_config_file = ConfigFile::parse(&global_config_path)?;
        let mut project_config_file = match find_project_config_file() {
            Some(path) if path != global_config_path => ConfigFile::parse(&path)?,
            _ => ConfigFile::default(),
        };

        let mode = if cli.offline {
            Mode::Offline
        } else {
            let Some(auth_token) = cli
                .auth_token
                .or_else(|| std::env::var("SENTRY_AUTH_TOKEN").ok())
                .or_else(|| project_config_file.auth_token.take())
                .or_else(|| global_config_file.auth_token.take())
            else {
                bail!(
                    "No auth token provided. Pass it either via the `--auth-token` option or via the `SENTRY_AUTH_TOKEN` environment variable."
                );
            };

            let sentry_url = cli
                .url
                .as_deref()
                .or(project_config_file.url.as_deref())
                .or(global_config_file.url.as_deref())
                .unwrap_or(DEFAULT_URL);

            let sentry_url = Url::parse(sentry_url).context("Invalid sentry URL")?;
            let url = sentry_url.join("/api/0/").unwrap();

            let Some(org) = cli
                .org
                .or_else(|| project_config_file.org.take())
                .or_else(|| global_config_file.org.take())
            else {
                bail!(
                    "No organization provided. Pass it either via the `--org` option or put it in .symboliclirc."
                );
            };

            let Some(project) = cli
                .project
                .or_else(|| project_config_file.project.take())
                .or_else(|| global_config_file.project.take())
            else {
                bail!(
                    "No project provided. Pass it either via the `--project` option or put it in .symboliclirc."
                );
            };

            Mode::Online {
                base_url: url,
                org,
                project,
                auth_token,
                scraping_enabled: !cli.no_scrape,
            }
        };

        let symbolicator_config = {
            let mut sources = project_config_file.sources;
            sources.append(&mut global_config_file.sources);

            let cache_dir = project_config_file
                .cache_dir
                .or(global_config_file.cache_dir);

            if let Some(path) = cache_dir.as_ref() {
                std::fs::create_dir_all(path)?;
            }

            let mut caches = CacheConfigs::default();
            caches.downloaded.retry_misses_after = Some(Duration::ZERO);
            caches.derived.retry_misses_after = Some(Duration::ZERO);

            Config {
                sources: Arc::from(sources),
                cache_dir,
                connect_to_reserved_ips: true,
                caches,
                ..Default::default()
            }
        };

        let args = Settings {
            event_id: cli.event,
            symbolicator_config,
            output_format: cli.format,
            log_level: cli.log_level,
            mode,
            symbols: cli.symbols,
        };

        Ok(args)
    }
}

fn find_global_config_file() -> Result<PathBuf> {
    dirs::home_dir()
        .ok_or_else(|| anyhow!("Could not find home dir"))
        .map(|mut path| {
            path.push(CONFIG_RC_FILE_NAME);
            path
        })
}

fn find_project_config_file() -> Option<PathBuf> {
    std::env::current_dir().ok().and_then(|mut path| {
        loop {
            path.push(CONFIG_RC_FILE_NAME);
            if path.exists() {
                return Some(path);
            }
            path.set_file_name("symbolicli.toml");
            if path.exists() {
                return Some(path);
            }
            path.pop();
            if !path.pop() {
                return None;
            }
        }
    })
}
