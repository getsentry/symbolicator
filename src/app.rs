use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use actix::{Actor, Addr};
use actix_web::{server, App};
use failure::Fail;
use structopt::StructOpt;
use tokio_threadpool::ThreadPool;

use crate::actors::{
    cache::CacheActor, objects::ObjectsActor, symbolication::SymbolicationActor,
    symcaches::SymCacheActor, cficaches::CfiCacheActor,
};
use crate::config::{Config, ConfigError};
use crate::endpoints;
use crate::logging;
use crate::metrics;
use crate::middlewares::{ErrorHandlers, Metrics};

#[derive(Fail, Debug, derive_more::From)]
pub enum CliError {
    #[fail(display = "Failed loading config: {}", _0)]
    ConfigParsing(#[fail(cause)] ConfigError),

    #[fail(display = "Failed loading cache dirs: {}", _0)]
    CacheIo(#[fail(cause)] io::Error),
}

#[derive(StructOpt)]
struct Cli {
    /// Path to your configuration file.
    #[structopt(
        long = "config",
        short = "c",
        raw(global = "true"),
        value_name = "FILE"
    )]
    pub config: Option<PathBuf>,

    #[structopt(subcommand)]
    command: Command,
}

impl Cli {
    /// Returns the path to the configuration file.
    pub fn config(&self) -> Option<&Path> {
        self.config.as_ref().map(PathBuf::as_path)
    }
}

#[derive(StructOpt)]
#[structopt(bin_name = "symbolicator")]
enum Command {
    /// Run server
    #[structopt(name = "run")]
    Run,
}

#[derive(Clone)]
pub struct ServiceState {
    pub io_threadpool: Arc<ThreadPool>,
    pub symbolication: Addr<SymbolicationActor>,
}

pub type ServiceApp = App<ServiceState>;

/// CLI entrypoint
pub fn main() -> ! {
    match execute() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            logging::ensure_log_error(&error);
            std::process::exit(1);
        }
    }
}

/// Runs the main application.
fn execute() -> Result<(), CliError> {
    let cli = Cli::from_args();
    let config = Config::get(cli.config())?;

    let _sentry = sentry::init(config.sentry_dsn.as_ref().map(|x| &**x));
    logging::init_logging(&config);
    sentry::integrations::panic::register_panic_handler();

    match cli.command {
        Command::Run => run_server(config)?,
    }

    Ok(())
}

/// Starts all actors and HTTP server based on loaded config.
fn run_server(config: Config) -> Result<(), CliError> {
    if let Some(ref statsd) = config.metrics.statsd {
        metrics::configure_statsd(&config.metrics.prefix, statsd);
    }

    let sys = actix::System::new("symbolicator");

    let cpu_threadpool = Arc::new(ThreadPool::new());
    let io_threadpool = Arc::new(ThreadPool::new());

    let objects_cache_path = config.cache_dir.as_ref().map(|x| x.join("./objects/"));
    if let Some(ref objects_cache_path) = objects_cache_path {
        fs::create_dir_all(objects_cache_path)?;
    }
    let objects = ObjectsActor::new(
        CacheActor::new("objects", objects_cache_path).start(),
        io_threadpool.clone(),
    )
    .start();

    let symcache_path = config.cache_dir.as_ref().map(|x| x.join("./symcaches/"));
    if let Some(ref symcache_path) = symcache_path {
        fs::create_dir_all(symcache_path)?;
    }
    let symcaches = SymCacheActor::new(
        CacheActor::new("symcaches", symcache_path).start(),
        objects.clone(),
        cpu_threadpool.clone(),
    )
    .start();

    let cficache_path = config.cache_dir.as_ref().map(|x| x.join("./cficaches/"));
    if let Some(ref cficache_path) = cficache_path {
        fs::create_dir_all(cficache_path)?;
    }
    let cficaches = CfiCacheActor::new(
        CacheActor::new("cficaches", cficache_path).start(),
        objects,
        cpu_threadpool.clone(),
    )
    .start();

    let symbolication = SymbolicationActor::new(symcaches, cficaches, cpu_threadpool.clone()).start();

    let state = ServiceState {
        io_threadpool,
        symbolication,
    };

    fn get_app(state: ServiceState) -> ServiceApp {
        let mut app = App::with_state(state)
            .middleware(Metrics)
            .middleware(ErrorHandlers)
            .middleware(sentry_actix::SentryMiddleware::new());

        app = endpoints::healthcheck::register(app);
        app = endpoints::minidump::register(app);
        app = endpoints::requests::register(app);
        app = endpoints::symbolicate::register(app);
        app
    }

    server::new(move || get_app(state.clone()))
        .bind(&config.bind)
        .unwrap()
        .start();

    log::info!("Started http server: {}", config.bind);
    let _ = sys.run();
    Ok(())
}
