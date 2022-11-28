use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use symbolicator_service::config::Config;
use symbolicator_sources::SourceConfig;

use anyhow::{anyhow, bail, Context};
use clap::{Parser, ValueEnum};
use reqwest::Url;
use serde::Deserialize;

/// The default API URL
pub const DEFAULT_URL: &str = "https://sentry.io/";

/// The name of the configuration file.
pub const CONFIG_RC_FILE_NAME: &str = ".symboliclirc";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Outputs the entire symbolication result as JSON.
    Json,
    /// Outputs the crashed thread as a detailed list of frames.
    Pretty,
    /// Outputs the crashed thread as a table.
    Compact,
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
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
struct ConfigFile {
    pub org: Option<String>,
    pub project: Option<String>,
    pub url: Option<String>,
    pub auth_token: Option<String>,
    //pub cache_dir: Option<PathBuf>,
    //pub caches: CacheConfigs,
    pub sources: Vec<SourceConfig>,
}

impl ConfigFile {
    pub fn parse(path: &Path) -> anyhow::Result<Self> {
        match std::fs::File::open(path) {
            Ok(mut file) => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .context("Could not read configuration file")?;
                toml::de::from_slice(&buf).context("Could not parse configuration file")
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

#[derive(Clone, Debug)]
pub struct Settings {
    pub event_id: String,
    pub org: String,
    pub project: String,
    pub auth_token: String,
    pub base_url: reqwest::Url,
    pub symbolicator_config: Config,
    pub output_format: OutputFormat,
}

impl Settings {
    pub fn get() -> anyhow::Result<Self> {
        let cli = Cli::parse();

        let mut global_config_file = ConfigFile::parse(&find_global_config_file()?)?;
        let mut project_config_file = match find_project_config_file() {
            Some(path) => ConfigFile::parse(&path)?,
            None => ConfigFile::default(),
        };

        let Some(auth_token) = cli
            .auth_token
            .or_else(|| std::env::var("SENTRY_AUTH_TOKEN").ok())
            .or_else(|| project_config_file.auth_token.take())
            .or_else(|| global_config_file.auth_token.take()) else {
            bail!("No auth token provided. Pass it either via the `--auth-token` option or via the `SENTRY_AUTH_TOKEN` environment variable.");
    };

        let sentry_url = cli
            .url
            .as_deref()
            .or(project_config_file.url.as_deref())
            .or(global_config_file.url.as_deref())
            .unwrap_or(DEFAULT_URL);

        let sentry_url = Url::parse(sentry_url).context("Invalid sentry URL")?;
        let url = sentry_url.join("/api/0/").unwrap();

        let Some(org) = cli.org
            .or_else(|| project_config_file.org.take())
            .or_else(|| global_config_file.org.take()) else {
            bail!("No organization provided. Pass it either via the `--org` option or put it in .symboliclirc.");  
        };

        let Some(project) = cli.project
            .or_else(|| project_config_file.project.take())
            .or_else(|| global_config_file.project.take()) else {
            bail!("No project provided. Pass it either via the `--project` option or put it in .symboliclirc.");  
        };

        let symbolicator_config = {
            let mut sources = project_config_file.sources;
            sources.append(&mut global_config_file.sources);

            Config {
                sources: Arc::from(sources),
                ..Default::default()
            }
        };

        let args = Settings {
            event_id: cli.event,
            org,
            project,
            auth_token,
            base_url: url,
            symbolicator_config,
            output_format: cli.format,
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

fn find_project_config_file() -> Option<PathBuf> {
    std::env::current_dir().ok().and_then(|mut path| loop {
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
    })
}
