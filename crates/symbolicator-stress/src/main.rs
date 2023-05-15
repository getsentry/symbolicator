use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use humantime::parse_duration;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use structopt::StructOpt;

use symbolicator_service::config::Config as SymbolicatorConfig;
use symbolicator_service::services::download::SourceConfig;
use symbolicator_service::services::symbolication::{
    StacktraceOrigin, SymbolicateJsStacktraces, SymbolicateStacktraces, SymbolicationActor,
};
use symbolicator_service::types::{JsStacktrace, RawObjectInfo, RawStacktrace, Scope};
use tokio::sync::Semaphore;

#[derive(Debug, Deserialize, Serialize)]
struct WorkloadsConfig {
    workloads: Vec<Workload>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Workload {
    concurrency: usize,
    #[serde(flatten)]
    payload: Payload,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Payload {
    Minidump(PathBuf),
    Event(PathBuf),
    Js { source: PathBuf, event: PathBuf },
}

#[derive(Debug, Deserialize)]
struct EventFile {
    stacktraces: Vec<RawStacktrace>,
    modules: Vec<RawObjectInfo>,
}

#[derive(Debug, Deserialize)]
struct JsEventFile {
    stacktraces: Vec<JsStacktrace>,
    modules: Vec<RawObjectInfo>,
}

#[derive(Clone)]
struct MinidumpPayload {
    scope: Scope,
    minidump_file: PathBuf,
    sources: Arc<[SourceConfig]>,
}

enum ParsedPayload {
    Minidump(MinidumpPayload),
    Event(SymbolicateStacktraces),
    Js(symbolicator_test::Server, SymbolicateJsStacktraces),
}

/// Command line interface parser.
#[derive(StructOpt)]
struct Cli {
    /// Path to your configuration file.
    #[structopt(long = "config", short = "c", value_name = "FILE")]
    config: Option<PathBuf>,

    /// Path to the workload definition file.
    #[structopt(long = "workloads", short = "w", value_name = "FILE")]
    workloads: PathBuf,

    /// Duration of the stresstest.
    #[structopt(long = "duration", short = "d", parse(try_from_str = parse_duration))]
    duration: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::from_args();

    // parse configs
    let workloads_file =
        std::fs::File::open(cli.workloads).context("failed to open workloads file")?;
    let workloads: WorkloadsConfig =
        serde_yaml::from_reader(workloads_file).context("failed to parse workloads YAML")?;

    let config_path = cli.config;
    let service_config = SymbolicatorConfig::get(config_path.as_deref())?;

    // TODO: we want to profile the effect of tracing without actually outputting stuff
    // tracing_subscriber::fmt::init();

    // start symbolicator service
    let runtime = tokio::runtime::Handle::current();
    let (symbolication, _objects) =
        symbolicator_service::services::create_service(&service_config, runtime)
            .context("failed starting symbolication service")?;
    let symbolication = Arc::new(symbolication);

    // initialize workloads
    let workloads: Vec<_> = workloads
        .workloads
        .into_iter()
        .enumerate()
        .map(|(i, workload)| {
            let scope = Scope::Scoped(i.to_string());
            let sources = service_config.sources.clone();
            let payload = prepare_payload(scope, sources, workload.payload);
            (workload.concurrency, Arc::new(payload))
        })
        .collect();

    // warmup: run each workload once to make sure caches are warm
    {
        let start = Instant::now();

        let futures = workloads.iter().map(|(_, workload)| {
            let symbolication = Arc::clone(&symbolication);
            let workload = Arc::clone(workload);
            tokio::spawn(async move {
                process_payload(&symbolication, &workload).await;
            })
        });

        let _results = futures::future::join_all(futures).await;

        println!("Warmup: {:?}", start.elapsed());
    };
    println!();

    // run the workloads concurrently
    let mut tasks = Vec::with_capacity(workloads.len());
    for (concurrency, workload) in workloads.into_iter() {
        let start = Instant::now();
        let duration = cli.duration;
        let deadline = tokio::time::Instant::from_std(start + duration);
        let symbolication = Arc::clone(&symbolication);
        let workload = Arc::clone(&workload);

        let task = tokio::spawn(async move {
            let finished_tasks = Arc::new(AtomicUsize::new(0));
            let semaphore = Arc::new(Semaphore::new(concurrency));

            // See <https://docs.rs/tokio/latest/tokio/time/struct.Sleep.html#examples>
            let sleep = tokio::time::sleep_until(deadline);
            tokio::pin!(sleep);

            loop {
                if deadline.elapsed() > Duration::ZERO {
                    break;
                }
                tokio::select! {
                    permit = semaphore.clone().acquire_owned() => {
                        let workload = Arc::clone(&workload);
                        let symbolication = Arc::clone(&symbolication);
                        let finished_tasks = Arc::clone(&finished_tasks);

                        tokio::spawn(async move {
                            process_payload(&symbolication, &workload).await;

                            // TODO: maybe maintain a histogram?
                            finished_tasks.fetch_add(1, Ordering::Relaxed);

                            drop(permit);
                        });
                    }
                    _ = &mut sleep => {
                        break;
                    }
                }
            }

            // we only count finished tasks
            let ops = finished_tasks.load(Ordering::Relaxed);

            // by acquiring *all* the semaphores, we essentially wait for all outstanding tasks to finish
            let _permits = semaphore.acquire_many(concurrency as u32).await;

            (concurrency, ops)
        });
        tasks.push(task);
    }

    let finished_tasks = futures::future::join_all(tasks).await;

    for (i, task) in finished_tasks.into_iter().enumerate() {
        let (concurrency, ops) = task.unwrap();

        let ops_ps = ops as f32 / cli.duration.as_secs() as f32;
        println!("Workload {i} (concurrency: {concurrency}): {ops} operations, {ops_ps} ops/s");
    }

    Ok(())
}

fn prepare_payload(scope: Scope, sources: Arc<[SourceConfig]>, payload: Payload) -> ParsedPayload {
    match payload {
        Payload::Minidump(path) => ParsedPayload::Minidump(MinidumpPayload {
            scope,
            sources,

            minidump_file: path,
        }),
        Payload::Event(path) => {
            let EventFile {
                stacktraces,
                modules,
            } = read_json(path);
            let modules = modules.into_iter().map(From::from).collect();

            ParsedPayload::Event(SymbolicateStacktraces {
                scope,
                signal: None,
                sources,
                origin: StacktraceOrigin::Symbolicate,
                apply_source_context: true,

                stacktraces,
                modules,
            })
        }
        Payload::Js { source, event } => {
            let parent = source.parent().unwrap();
            let source: serde_json::Value = read_json(&source);
            let (srv, source) = symbolicator_test::sourcemap_server(parent, move |url, _query| {
                let lookup = source
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| {
                        let mut entry = entry.clone();
                        let map = entry.as_object_mut().unwrap();
                        let url = map["url"].as_str().unwrap().replace("{url}", url);
                        map["url"] = serde_json::Value::String(url);
                        entry
                    })
                    .collect();
                serde_json::Value::Array(lookup)
            });

            let JsEventFile {
                stacktraces,
                modules,
            } = read_json(event);

            ParsedPayload::Js(
                srv,
                SymbolicateJsStacktraces {
                    scope,
                    source: Arc::new(source),
                    release: Some("some-release".into()),
                    dist: None,
                    stacktraces,
                    modules,
                    scraping: Default::default(),
                    apply_source_context: true,
                },
            )
        }
    }
}

fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> T {
    let file = std::fs::File::open(path).unwrap();
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).unwrap()
}

async fn process_payload(symbolication: &SymbolicationActor, workload: &ParsedPayload) {
    match workload {
        ParsedPayload::Minidump(payload) => {
            let MinidumpPayload {
                scope,
                minidump_file,
                sources,
            } = payload;

            // processing a minidump requires a tempfile that can be persisted -_-
            // so that means we have to make a copy of our minidump
            let mut temp_file = tempfile::Builder::new();
            temp_file.prefix("minidump").suffix(".dmp");
            let temp_file = temp_file.tempfile().unwrap();
            let (temp_file, temp_path) = temp_file.into_parts();
            let mut temp_file = tokio::fs::File::from_std(temp_file);

            let mut minidump_file = tokio::fs::File::open(minidump_file).await.unwrap();

            tokio::io::copy(&mut minidump_file, &mut temp_file)
                .await
                .unwrap();

            symbolication
                .process_minidump(scope.clone(), temp_path, Arc::clone(sources))
                .await
                .unwrap();
        }
        ParsedPayload::Event(payload) => {
            symbolication.symbolicate(payload.clone()).await.unwrap();
        }
        ParsedPayload::Js(_srv, payload) => {
            symbolication.symbolicate_js(payload.clone()).await.unwrap();
        }
    };
}
