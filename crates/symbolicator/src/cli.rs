//! Exposes the command line application.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use structopt::StructOpt;
use tracing_subscriber::prelude::*;

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

#[derive(Default)]
struct SaveToDiskTransport {
    n: AtomicUsize,
}

impl sentry::Transport for SaveToDiskTransport {
    fn send_envelope(&self, envelope: sentry::Envelope) {
        let num = self.n.fetch_add(1, Ordering::Relaxed);
        let file_name = format!("envelope-{}.txt", num);
        let mut file = std::fs::File::create(file_name).unwrap();
        envelope.to_writer(&mut file).unwrap();
    }
}

/// Runs the main application.
pub fn execute() -> Result<()> {
    let cli = Cli::from_args();
    let config = Config::get(cli.config()).context("failed loading config")?;

    let _sentry = sentry::init(sentry::ClientOptions {
        dsn: config.sentry_dsn.clone(),
        release: Some(env!("SYMBOLICATOR_RELEASE").into()),
        session_mode: sentry::SessionMode::Request,
        auto_session_tracking: false,
        traces_sample_rate: 1.0,
        //transport: Some(Arc::new(Arc::new(SaveToDiskTransport::default()))),
        ..Default::default()
    });

    if std::env::var("RUST_LOG").is_err() {
        let rust_log = logging::get_rust_log(config.logging.level);
        std::env::set_var("RUST_LOG", rust_log);
    }
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);
    let filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
        .unwrap();

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(filter_layer)
        .with(sentry::integrations::tracing::layer())
        .init();

    logging::init_logging(&config);

    if let Some(ref statsd) = config.metrics.statsd {
        let hostname = config.metrics.hostname_tag.clone().and_then(|tag| {
            hostname::get()
                .ok()
                .and_then(|s| s.into_string().ok())
                .map(|name| (tag, name))
        });
        metrics::configure_statsd(&config.metrics.prefix, statsd, hostname);
    }

    procspawn::ProcConfig::new()
        .config_callback(|| {
            log::trace!("[procspawn] initializing in sub process");
            metric!(counter("procspawn.init") += 1);
        })
        .init();

    match cli.command {
        Command::Run => server::run(config).context("failed to start the server")?,
        Command::Cleanup => cache::cleanup(config).context("failed to clean up caches")?,
    }

    Ok(())
}
