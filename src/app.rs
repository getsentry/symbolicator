use crate::config::{get_config, Config, ConfigError};
use actix::{self, Actor, Addr};
use actix_web::{server, App};
use std::path::PathBuf;

use structopt::StructOpt;

use crate::{
    actors::{cache::CacheActor, objects::ObjectsActor, symbolication::SymbolicationActor},
    endpoints,
};

use failure::Fail;

#[derive(Fail, Debug, derive_more::From)]
pub enum CliError {
    #[fail(display = "Failed loading config: {}", _0)]
    ConfigParsing(#[fail(cause)] ConfigError),
}

#[derive(StructOpt)]
struct Cli {
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
    let config = get_config(cli.config)?;

    match cli.command {
        Command::Run => run_server(config)?,
    }

    Ok(())
}

pub fn run_server(config: Config) -> Result<(), CliError> {
    let sys = actix::System::new("symbolicator");

    let download_cache_path = config.cache_dir.as_ref().map(|x| x.join("./objects"));
    let download_cache = CacheActor::new(download_cache_path).start();
    let objects = ObjectsActor::new(download_cache).start();

    let sym_cache_path = config.cache_dir.as_ref().map(|x| x.join("./symcaches"));
    let sym_cache = CacheActor::new(sym_cache_path).start();
    let symbolication = SymbolicationActor::new(sym_cache, objects).start();

    let state = ServiceState { symbolication };

    fn get_app(state: ServiceState) -> ServiceApp {
        let mut app = App::with_state(state);
        app = endpoints::symbolicate::register(app);
        app = endpoints::healthcheck::register(app);
        app
    }

    server::new(move || get_app(state.clone()))
        .bind(
            config
                .bind
                .as_ref()
                .map(|x| &**x)
                .unwrap_or("127.0.0.1:8080"),
        )
        .unwrap()
        .start();

    println!("Started http server: 127.0.0.1:8080");
    let _ = sys.run();
    Ok(())
}
