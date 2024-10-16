//! Exposes the command line application.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{arg, Parser};

use symbolicator_service::caching;
use symbolicator_service::metrics;

use crate::config::Config;
use crate::logging;
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
#[derive(Parser)]
#[command(bin_name = "symbolicator")]
enum Command {
    /// Run the web server.
    #[command(name = "run")]
    Run,

    /// Clean local caches.
    #[command(name = "cleanup")]
    Cleanup {
        /// Only simulate the cleanup without deleting any files.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Command line interface parser.
#[derive(Parser)]
#[command(
    version = get_crate_version(),
    long_version = get_long_crate_version(),
)]
struct Cli {
    /// Path to your configuration file.
    #[arg(long = "config", short = 'c', global(true), value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
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
    let cli = Cli::parse();
    let config = Config::get(cli.config()).context("failed loading config")?;

    let release = Some(env!("SYMBOLICATOR_RELEASE").into());

    #[cfg(feature = "symbolicator-crash")]
    {
        let dsn = config.sentry_dsn.as_ref().map(|d| d.to_string());
        let db = config._crash_db.clone().or_else(|| {
            config
                .cache_dir
                .as_ref()
                .map(|cache_dir| cache_dir.join(".sentry-native"))
        });
        if let (Some(dsn), Some(db)) = (dsn, db) {
            symbolicator_crash::CrashHandler::new(dsn.as_ref(), &db)
                .release(release.as_deref())
                .install();
        }
    }
    let sentry = sentry::init(sentry::ClientOptions {
        dsn: config.sentry_dsn.clone(),
        release,
        session_mode: sentry::SessionMode::Request,
        auto_session_tracking: false,
        traces_sampler: Some(Arc::new(move |ctx| {
            if Some(true) == ctx.sampled() && config.propagate_traces {
                1.0
            } else if ctx.operation() != "http.server" {
                config.traces_sample_rate
            } else {
                0.0
            }
        })),
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

        if let Some(platform_tag) = config.metrics.platform_tag.clone() {
            if tags.contains_key(&platform_tag) {
                tracing::warn!(
                    "tag {} defined both as platform tag and as a custom tag",
                    platform_tag
                );
            }
            match std::env::var("SYMBOLICATOR_PLATFORM") {
                Ok(platform) => {
                    tags.insert(platform_tag, platform.to_string());
                }
                Err(e) => {
                    tracing::error!(error = %e, "platform not available");
                }
            }
        };

        metrics::configure_statsd(&config.metrics.prefix, statsd, tags);
    }

    match cli.command {
        Command::Run => server::run(config).context("failed to start the server")?,
        Command::Cleanup { dry_run } => {
            caching::cleanup(config, dry_run).context("failed to clean up caches")?
        }
    }

    Ok(())
}
