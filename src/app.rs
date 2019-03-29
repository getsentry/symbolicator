use std::env;
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
    symcaches::SymCacheActor,
};
use crate::config::{Config, ConfigError};
use crate::endpoints;
use crate::metrics;
use crate::middlewares::Metrics;

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
    pub symbolication: Addr<SymbolicationActor>,
}

pub type ServiceApp = App<ServiceState>;

pub fn run_main() -> Result<(), CliError> {
    env_logger::init();
    let cli = Cli::from_args();
    let config = Config::get(cli.config())?;

    let _sentry = sentry::init(config.sentry_dsn.as_ref().map(|x| &**x));
    env::set_var("RUST_BACKTRACE", "1");
    sentry::integrations::panic::register_panic_handler();

    match cli.command {
        Command::Run => run_server(config)?,
    }

    Ok(())
}

pub fn run_server(config: Config) -> Result<(), CliError> {
    if let Some(ref statsd) = config.metrics.statsd {
        metrics::configure_statsd(&config.metrics.prefix, statsd);
    }

    let sys = actix::System::new("symbolicator");

    let cpu_threadpool = Arc::new(ThreadPool::new());
    let io_threadpool = Arc::new(ThreadPool::new());

    let download_cache_path = config.cache_dir.as_ref().map(|x| x.join("./objects/"));
    if let Some(ref download_cache_path) = download_cache_path {
        fs::create_dir_all(download_cache_path)?;
    }
    let download_cache = CacheActor::new(download_cache_path).start();
    let objects = ObjectsActor::new(download_cache, io_threadpool.clone()).start();

    let symcache_path = config.cache_dir.as_ref().map(|x| x.join("./symcaches/"));
    if let Some(ref symcache_path) = symcache_path {
        fs::create_dir_all(symcache_path)?;
    }
    let symcache_cache = CacheActor::new(symcache_path).start();
    let symcaches = SymCacheActor::new(symcache_cache, objects, cpu_threadpool.clone()).start();

    let symbolication = SymbolicationActor::new(symcaches, cpu_threadpool.clone()).start();

    let state = ServiceState { symbolication };

    fn get_app(state: ServiceState) -> ServiceApp {
        let mut app = App::with_state(state)
            .middleware(Metrics)
            .middleware(sentry_actix::SentryMiddleware::new());

        app = endpoints::symbolicate::register(app);
        app = endpoints::healthcheck::register(app);
        app = endpoints::requests::register(app);
        app
    }

    server::new(move || get_app(state.clone()))
        .bind(&config.bind)
        .unwrap()
        .start();

    println!("Started http server: {}", config.bind);
    let _ = sys.run();
    Ok(())
}
