//! Exposes the command line application.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use actix::Actor;
use actix_web::{client, server, App};
use failure::Fail;
use futures::future;
use structopt::StructOpt;
use tokio_threadpool::ThreadPool;

use crate::actors::{
    cficaches::CfiCacheActor, objects::ObjectsActor, symbolication::SymbolicationActor,
    symcaches::SymCacheActor,
};
use crate::cache::{Cache, CleanupError};
use crate::config::{Config, ConfigError};
use crate::endpoints;
use crate::http::SafeResolver;
use crate::logging;
use crate::metrics;
use crate::middlewares::{ErrorHandlers, Metrics};

/// An enum representing a CLI error.
#[derive(Fail, Debug, derive_more::From)]
pub enum CliError {
    /// Indicates a config parsing error.
    #[fail(display = "Failed loading config")]
    ConfigParsing(#[fail(cause)] ConfigError),

    /// Indicates an IO error accessing the cache.
    #[fail(display = "Failed loading cache dirs")]
    CacheIo(#[fail(cause)] io::Error),

    /// Indicates an error while cleaning up caches.
    #[fail(display = "Failed cleaning up caches")]
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

    /// Clean up caches
    #[structopt(name = "cleanup")]
    Cleanup,
}

/// The shared state for the service.
#[derive(Clone, Debug)]
pub struct ServiceState {
    /// Thread pool instance reserved for CPU-intensive tasks.
    pub cpu_threadpool: Arc<ThreadPool>,
    /// Thread pool instance reserved for IO-intensive tasks.
    pub io_threadpool: Arc<ThreadPool>,
    /// Actor for minidump and stacktrace processing
    pub symbolication: Arc<SymbolicationActor>,
    /// Actor for downloading and caching objects (no symcaches or cficaches)
    pub objects: Arc<ObjectsActor>,
    /// The config object.
    pub config: Arc<Config>,
}

/// Typedef for the application type.
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

    let _sentry = sentry::init(config.sentry_dsn.clone());
    logging::init_logging(&config);
    sentry::integrations::panic::register_panic_handler();

    match cli.command {
        Command::Run => run_server(config)?,
        Command::Cleanup => cleanup_caches(config)?,
    }

    Ok(())
}

struct Caches {
    objects: Cache,
    object_meta: Cache,
    symcaches: Cache,
    cficaches: Cache,
}

impl Caches {
    fn new(config: &Config) -> Self {
        Caches {
            objects: {
                let path = config.cache_dir.as_ref().map(|x| x.join("./objects/"));
                Cache::new("objects", path, config.caches.downloaded)
            },
            object_meta: {
                let path = config.cache_dir.as_ref().map(|x| x.join("./object_meta/"));
                Cache::new("object_meta", path, config.caches.derived)
            },
            symcaches: {
                let path = config.cache_dir.as_ref().map(|x| x.join("./symcaches/"));
                Cache::new("symcaches", path, config.caches.derived)
            },
            cficaches: {
                let path = config.cache_dir.as_ref().map(|x| x.join("./cficaches/"));
                Cache::new("cficaches", path, config.caches.derived)
            },
        }
    }
}

fn cleanup_caches(config: Config) -> Result<(), CliError> {
    let caches = Caches::new(&config);
    caches.objects.cleanup()?;
    caches.object_meta.cleanup()?;
    caches.symcaches.cleanup()?;
    caches.cficaches.cleanup()?;
    Ok(())
}

fn get_system(config: Config) -> (actix::SystemRunner, ServiceState) {
    let config = Arc::new(config);

    if let Some(ref statsd) = config.metrics.statsd {
        metrics::configure_statsd(&config.metrics.prefix, statsd);
    }

    let caches = Caches::new(&config);

    let mut sys = actix::System::new("symbolicator");

    if !config.connect_to_reserved_ips {
        sys.block_on(future::lazy(|| {
            actix::System::current().registry().set(
                client::ClientConnector::default()
                    .resolver(SafeResolver::default().start().recipient())
                    .start(),
            );
            Ok::<(), ()>(())
        }))
        .unwrap();
    }

    let cpu_threadpool = Arc::new(ThreadPool::new());
    let io_threadpool = Arc::new(ThreadPool::new());

    let objects = Arc::new(ObjectsActor::new(
        caches.object_meta,
        caches.objects,
        io_threadpool.clone(),
    ));

    let symcaches = Arc::new(SymCacheActor::new(
        caches.symcaches,
        objects.clone(),
        cpu_threadpool.clone(),
    ));

    let cficaches = Arc::new(CfiCacheActor::new(
        caches.cficaches,
        objects.clone(),
        cpu_threadpool.clone(),
    ));

    let symbolication = Arc::new(SymbolicationActor::new(
        objects.clone(),
        symcaches,
        cficaches,
        cpu_threadpool.clone(),
    ));

    let state = ServiceState {
        cpu_threadpool,
        io_threadpool,
        symbolication,
        objects,
        config: config.clone(),
    };

    (sys, state)
}

#[cfg(test)]
pub(crate) fn get_test_system() -> (actix::SystemRunner, ServiceState) {
    get_system(Default::default())
}

#[cfg(test)]
pub(crate) fn get_test_system_with_cache() -> (tempfile::TempDir, actix::SystemRunner, ServiceState)
{
    let tempdir = tempfile::TempDir::new().unwrap();
    let (runner, state) = get_system(Config {
        cache_dir: Some(tempdir.path().to_owned()),
        ..Default::default()
    });

    (tempdir, runner, state)
}

/// Starts all actors and HTTP server based on loaded config.
fn run_server(config: Config) -> Result<(), CliError> {
    let (sys, state) = get_system(config);

    fn get_app(state: ServiceState) -> ServiceApp {
        let mut app = App::with_state(state)
            .middleware(Metrics)
            .middleware(ErrorHandlers)
            .middleware(sentry_actix::SentryMiddleware::new());

        app = endpoints::applecrashreport::register(app);
        app = endpoints::healthcheck::register(app);
        app = endpoints::minidump::register(app);
        app = endpoints::requests::register(app);
        app = endpoints::symbolicate::register(app);
        app = endpoints::proxy::register(app);
        app
    }

    metric!(counter("server.starting") += 1);
    server::new(clone!(state, || get_app(state.clone())))
        .bind(&state.config.bind)?
        .start();

    log::info!("Started http server: {}", state.config.bind);

    let _ = sys.run();
    Ok(())
}
