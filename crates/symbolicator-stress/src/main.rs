use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use humantime::parse_duration;

use symbolicator_service::config::Config as SymbolicatorConfig;

mod logging;
mod stresstest;
mod workloads;

use stresstest::perform_stresstest;
use workloads::WorkloadsConfig;

#[cfg(not(target_env = "msvc"))]
use jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// Command line interface parser.
#[derive(Parser)]
struct Cli {
    /// Path to your configuration file.
    #[arg(long = "config", short = 'c', value_name = "FILE")]
    config: Option<PathBuf>,

    /// Path to the workload definition file.
    #[arg(long = "workloads", short = 'w', value_name = "FILE")]
    workloads: PathBuf,

    /// Duration of the stresstest.
    #[arg(long = "duration", short = 'd', value_parser = parse_duration)]
    duration: Duration,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // parse configs
    let workloads_file =
        std::fs::File::open(cli.workloads).context("failed to open workloads file")?;
    let workloads: WorkloadsConfig =
        serde_yaml::from_reader(workloads_file).context("failed to parse workloads YAML")?;

    let config_path = cli.config;
    let service_config = SymbolicatorConfig::get(config_path.as_deref())?;

    let mut logging_guard = logging::init(logging::Config {
        // TODO: actually make this configurable
        backtraces: true,
        tracing: true,
        sentry: true,
        metrics: true,
    });

    let megs = 1024 * 1024;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * megs)
        .build()?;

    if let Some(http_sink) = logging_guard.http_sink.take() {
        runtime.spawn(http_sink);
    }
    if let Some(udp) = logging_guard.upd_sink.take() {
        runtime.spawn(udp);
    }

    runtime.block_on(perform_stresstest(service_config, workloads, cli.duration))?;

    Ok(())
}
