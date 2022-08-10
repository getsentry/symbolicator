//! Exposes the command line application.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use structopt::StructOpt;

use crate::cache;
use crate::config::Config;
use crate::logging;
use crate::metrics;
use crate::server;

fn get_crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn get_long_crate_version() -> &'static str {
    concat!(
        "version: ",
        env!("CARGO_PKG_VERSION"),
        "\ngit commit: ",
        env!("SYMBOLICATOR_GIT_VERSION")
    )
}

/// Symbolicator commands.
#[derive(StructOpt)]
#[structopt(bin_name = "symbolicator")]
enum Command {
    /// Run the web server.
    #[structopt(name = "run")]
    Run,

    /// Clean local caches.
    #[structopt(name = "cleanup")]
    Cleanup,
}

/// Command line interface parser.
#[derive(StructOpt)]
#[structopt(
    version = get_crate_version(),
    long_version = get_long_crate_version(),
)]
struct Cli {
    /// Path to your configuration file.
    #[structopt(long = "config", short = "c", global(true), value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[structopt(subcommand)]
    command: Command,
}

impl Cli {
    /// Returns the path to the configuration file.
    fn config(&self) -> Option<&Path> {
        self.config.as_deref()
    }
}

/// Runs the main application.
pub fn execute() -> Result<()> {
    let cli = Cli::from_args();
    let config = Config::get(cli.config()).context("failed loading config")?;

    #[cfg(feature = "symbolicator-crash")]
    {
        if let Some(dsn) = config.sentry_dsn.clone().map(|d| d.to_string()) {
            if let Some(db) = config._crash_db.as_deref() {
                symbolicator_crash::CrashHandler::new(dsn.as_ref(), db)
                    .release(sentry::release_name!().as_deref())
                    .install();
            }
        }
    }
    let sentry = sentry::init(sentry::ClientOptions {
        dsn: config.sentry_dsn.clone(),
        release: Some(env!("SYMBOLICATOR_RELEASE").into()),
        session_mode: sentry::SessionMode::Request,
        auto_session_tracking: false,
        enable_profiling: true,
        // profiles are sub-sampling transactions, which we only sample based on upstream requests.
        // the sample rate we get from upstream is very low (< 20 transactions per minute, spread
        // across ~30 somewhat instances), so we might as well go full blast here.
        profiles_sample_rate: 1.0,
        ..Default::default()
    });

    logging::init_logging(&config);
    if let Some(ref statsd) = config.metrics.statsd {
        let mut tags = config.metrics.custom_tags.clone();

        if let Some(hostname_tag) = config.metrics.hostname_tag.clone() {
            if tags.contains_key(&hostname_tag) {
                tracing::warn!(
                    "tag {} defined both as hostname tag and as a custom tag",
                    hostname_tag
                );
            }
            if let Some(hostname) = hostname::get().ok().and_then(|s| s.into_string().ok()) {
                tags.insert(hostname_tag, hostname);
            } else {
                tracing::error!("could not read host name");
            }
        };
        if let Some(environment_tag) = config.metrics.environment_tag.clone() {
            if tags.contains_key(&environment_tag) {
                tracing::warn!(
                    "tag {} defined both as environment tag and as a custom tag",
                    environment_tag
                );
            }
            if let Some(environment) = sentry.options().environment.as_ref().map(|s| s.to_string())
            {
                tags.insert(environment_tag, environment);
            } else {
                tracing::error!("environment name not available");
            }
        };

        metrics::configure_statsd(&config.metrics.prefix, statsd, tags);
    }

    match cli.command {
        Command::Run => server::run(config).context("failed to start the server")?,
        Command::Cleanup => cache::cleanup(config).context("failed to clean up caches")?,
    }

    Ok(())
}
