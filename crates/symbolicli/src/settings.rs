use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use symbolicator_service::config::{CacheConfigs, Config};
use symbolicator_sources::SourceConfig;

use anyhow::{anyhow, bail, Context};
use clap::Parser;
use reqwest::Url;
use serde::Deserialize;

/// The default API URL
pub const DEFAULT_URL: &str = "https://sentry.io/";

/// The name of the configuration file.
pub const CONFIG_RC_FILE_NAME: &str = ".symboliclirc";

/// A utility that provides local symbolication of Sentry events.
///
/// A valid auth token needs to be provided via the `--auth-token` option,
/// the `SENTRY_AUTH_TOKEN` environment variable, or `~/.symboliclirc`.
///
/// The symbolication result will be returned as JSON.
#[derive(Clone, Parser, Debug)]
#[command(author, version, about, long_about)]
struct Cli {
    /// The ID of the event to symbolicate.
    pub event: String,

    /// The organization slug.
    #[arg(long, short)]
    pub org: Option<String>,

    /// The project slug.
    #[arg(long, short)]
    pub project: Option<String>,

    /// The URL of the sentry instance to connect to.
    ///
    /// Defaults to https://sentry.io/.
    #[arg(long)]
    pub url: Option<String>,

    /// The Sentry auth token to use to access the event and DIFs.
    ///
    /// This can alternatively be passed via the `SENTRY_AUTH_TOKEN` environment variable
    /// or `~/.symboliclirc`.
    #[arg(long = "auth-token")]
    pub auth_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ConfigFile {
    pub org: Option<String>,
    pub project: Option<String>,
    pub url: Option<String>,
    #[serde(rename = "auth-token")]
    pub auth_token: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub caches: CacheConfigs,
    pub sources: Arc<[SourceConfig]>,
}

impl ConfigFile {
    pub fn parse(path: &Path) -> anyhow::Result<Self> {
        match std::fs::File::open(path) {
            Ok(mut file) => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .context("Could not open configuration file")?;
                toml::de::from_slice(&buf).context("Could not read configuration file")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "Configuration file not found");
                Ok(Self::default())
            }
            Err(e) => Err(e).context(format!(
                "Could not open configuration file at {}",
                path.display()
            )),
        }
    }
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            org: None,
            project: None,
            url: None,
            auth_token: None,
            cache_dir: None,
            caches: Default::default(),
            sources: Arc::from(vec![]),
        }
    }
}

impl From<ConfigFile> for Config {
    fn from(config_file: ConfigFile) -> Self {
        Self {
            cache_dir: config_file.cache_dir,
            caches: config_file.caches,
            sources: config_file.sources,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub event_id: String,
    pub org: String,
    pub project: String,
    pub auth_token: String,
    pub base_url: reqwest::Url,
    pub symbolicator_config: Config,
}

impl Settings {
    pub fn get() -> anyhow::Result<Self> {
        let cli = Cli::parse();

        let mut config_file = ConfigFile::parse(&find_global_config_file()?)?;

        let Some(auth_token) = cli
            .auth_token
            .or_else(|| std::env::var("SENTRY_AUTH_TOKEN").ok())
            .or_else(|| config_file.auth_token.take()) else {
            bail!("No auth token provided. Pass it either via the `--auth-token` option or via the `SENTRY_AUTH_TOKEN` environment variable.");
    };

        let sentry_url = cli
            .url
            .as_deref()
            .or(config_file.url.as_deref())
            .unwrap_or(DEFAULT_URL);

        let sentry_url = Url::parse(sentry_url).context("Invalid sentry URL")?;
        let url = if sentry_url.as_str().ends_with('/') {
            sentry_url.join("api/0/").unwrap()
        } else {
            sentry_url.join("/api/0/").unwrap()
        };

        let Some(org) = cli.org.or_else(|| config_file.org.take()) else {
            bail!("No organization provided. Pass it either via the `--org` option or put it in .symboliclirc.");  
        };

        let Some(project) = cli.project.or_else(|| config_file.project.take()) else {
            bail!("No project provided. Pass it either via the `--project` option or put it in .symboliclirc.");  
        };

        let symbolicator_config = config_file.into();

        let args = Settings {
            event_id: cli.event,
            org,
            project,
            auth_token,
            base_url: url,
            symbolicator_config,
        };

        Ok(args)
    }
}

fn find_global_config_file() -> anyhow::Result<PathBuf> {
    dirs::home_dir()
        .ok_or_else(|| anyhow!("Could not find home dir"))
        .map(|mut path| {
            path.push(CONFIG_RC_FILE_NAME);
            path
        })
}
