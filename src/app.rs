//! Exposes the command line application.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use actix::{Actor, Addr};
use actix_web::{server, App};
use failure::Fail;
use futures::{future, Future};
use structopt::StructOpt;
use tokio_threadpool::ThreadPool;

use crate::actors::{
    cache::CacheActor, cficaches::CfiCacheActor, objects::ObjectsActor,
    symbolication::SymbolicationActor, symcaches::SymCacheActor,
};
use crate::config::{Config, ConfigError};
use crate::endpoints;
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

    /// A source was not found.
    #[fail(display = "Source not found in config")]
    SourceNotFound,

    /// A source is not supported for this operation
    #[fail(display = "Source not supported for this operation")]
    UnsupportedSource,
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
#[structopt(raw(version = "get_crate_version()"))]
#[structopt(raw(long_version = "get_long_crate_version()"))]
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
    /// Uploads debug information files to a source store
    #[structopt(name = "upload")]
    Upload(UploadCommand),
}

#[derive(StructOpt)]
struct UploadCommand {
    /// The store to upload to
    #[structopt(long = "store", short = "s", value_name = "ID")]
    store: String,
    /// The files to upload
    #[structopt(index = 1, value_name = "PATH")]
    files: Vec<PathBuf>,
}

/// The shared state for the service.
#[derive(Clone)]
pub struct ServiceState {
    /// Thread pool instance reserved for IO-intensive tasks.
    pub io_threadpool: Arc<ThreadPool>,
    /// The address of the symbolication actor.
    pub symbolication: Addr<SymbolicationActor>,
    /// The address of the objects actor.
    pub objects: Addr<ObjectsActor>,
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
        Command::Upload(args) => upload_files(config, args)?,
    }

    Ok(())
}

fn upload_files(config: Config, args: UploadCommand) -> Result<(), CliError> {
    use crate::actors::objects::s3::upload_file;
    use crate::types::SourceConfig;
    use crate::utils::paths::get_directory_path_for_object;
    use symbolic::common::ByteView;
    use symbolic::debuginfo::{Archive, FileFormat};

    let source = config
        .sources
        .iter()
        .find(|x| x.id() == args.store)
        .ok_or(CliError::SourceNotFound)?
        .clone();
    if !source.is_writable() {
        return Err(CliError::UnsupportedSource);
    };

    let sys = actix::System::new("symbolicator-upload");

    actix::spawn(future::lazy(move || {
        let mut futures = vec![];
        for root in args.files {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
            {
                if !entry.metadata().ok().map_or(false, |x| x.is_file()) {
                    continue;
                }
                let buffer = match ByteView::open(entry.path()) {
                    Ok(buffer) => buffer,
                    Err(_) => continue,
                };
                if Archive::peek(&buffer) == FileFormat::Unknown {
                    continue;
                }

                let archive = match Archive::parse(&buffer) {
                    Ok(archive) => archive,
                    Err(err) => {
                        log::error!("could not load object {}: {}", entry.path().display(), err);
                        continue;
                    }
                };

                let mut target_keys = vec![];
                for obj in archive.objects() {
                    let obj = match obj {
                        Ok(obj) => {
                            if obj.debug_id().is_nil() {
                                continue;
                            }
                            obj
                        }
                        Err(err) => {
                            log::error!(
                                "could not load contained object {}: {}",
                                entry.path().display(),
                                err
                            );
                            continue;
                        }
                    };

                    let key = match get_directory_path_for_object(
                        source.directory_layout(),
                        Some(entry.path()),
                        &obj,
                    ) {
                        Some(key) => key,
                        None => {
                            log::info!(
                                "skipping {}; target store does not support this format",
                                entry.path().display(),
                            );
                            continue;
                        }
                    };
                    target_keys.push(key);
                }

                for key in target_keys {
                    log::info!("Uploading {}", key);
                    let s3_config = match source {
                        SourceConfig::S3(ref s3_config) => s3_config.clone(),
                        _ => continue,
                    };
                    futures.push(upload_file(&s3_config, key, buffer.clone()));
                }
            }
        }

        future::join_all(futures)
            .map_err(|err| log::error!("upload failed: {}", err))
            .then(|_| {
                actix::System::current().stop();
                Ok(())
            })
    }));

    let _ = sys.run();
    Ok(())
}

/// Starts all actors and HTTP server based on loaded config.
fn run_server(config: Config) -> Result<(), CliError> {
    let config = Arc::new(config);
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
        objects.clone(),
        cpu_threadpool.clone(),
    )
    .start();

    let symbolication =
        SymbolicationActor::new(symcaches, cficaches, cpu_threadpool.clone()).start();

    let state = ServiceState {
        io_threadpool,
        symbolication,
        objects,
        config: config.clone(),
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
        app = endpoints::proxy::register(app);
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
