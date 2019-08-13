//! Exposes the command line application.
use std::path::{Path, PathBuf};

use failure::Fail;
use structopt::StructOpt;

use crate::cache::{self, CleanupError};
use crate::config::{Config, ConfigError};
use crate::logging;
use crate::server::{self, ServerError};

/// An enum representing a CLI error.
#[derive(Fail, Debug, derive_more::From)]
pub enum CliError {
    /// Indicates a config parsing error.
    #[fail(display = "Failed loading config")]
    Config(#[fail(cause)] ConfigError),

    /// Indicates an error starting the server.
    #[fail(display = "Failed start the server")]
    Server(#[fail(cause)] ServerError),

    /// Indicates an error while cleaning up caches.
    #[fail(display = "Failed to clean up caches")]
    Cleanup(#[fail(cause)] CleanupError),
}

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
#[structopt(raw(version = "get_crate_version()"))]
#[structopt(raw(long_version = "get_long_crate_version()"))]
struct Cli {
    /// Path to the configuration file.
    #[structopt(
        long = "config",
        short = "c",
        raw(global = "true"),
        value_name = "FILE"
    )]
    config: Option<PathBuf>,

    /// The command to execute.
    #[structopt(subcommand)]
    command: Command,
}

impl Cli {
    /// Returns the path to the configuration file.
    fn config(&self) -> Option<&Path> {
        self.config.as_ref().map(PathBuf::as_path)
    }
}

/// Runs the main application.
pub fn execute() -> Result<(), CliError> {
    let cli = Cli::from_args();
    let config = Config::get(cli.config())?;

    let _sentry = sentry::init(sentry::ClientOptions {
        dsn: config.sentry_dsn.clone(),
        release: Some(env!("SYMBOLICATOR_GIT_VERSION").into()),
        ..Default::default()
    });

    logging::init_logging(&config);
    sentry::integrations::panic::register_panic_handler();

    match cli.command {
        Command::Run => server::run(config)?,
        Command::Cleanup => cache::cleanup(config)?,
    }

    Ok(())
}
