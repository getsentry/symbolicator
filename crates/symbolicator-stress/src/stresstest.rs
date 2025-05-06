use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use sentry::SentryFutureExt;
use sketches_ddsketch::DDSketch;
use symbolicator_js::SourceMapService;
use symbolicator_native::SymbolicationActor;
use symbolicator_service::config::Config as SymbolicatorConfig;
use symbolicator_service::services::SharedServices;
use symbolicator_service::types::Scope;
use tokio::sync::Semaphore;

use crate::workloads::{WorkloadsConfig, prepare_payload, process_payload};

pub async fn perform_stresstest(
    service_config: SymbolicatorConfig,
    workloads: WorkloadsConfig,
    duration: Duration,
) -> Result<()> {
    // start symbolicator service
    let runtime = tokio::runtime::Handle::current();
    let shared_services = SharedServices::new(service_config, runtime)
        .context("failed to start symbolication service")?;
    let native = SymbolicationActor::new(&shared_services);
    let js = SourceMapService::new(&shared_services);
    let symbolication = Arc::new((native, js));
    let service_config = shared_services.config;

    // initialize workloads
    let workloads: Vec<_> = workloads
        .workloads
        .into_iter()
        .enumerate()
        .map(|(i, workload)| {
            let scope = Scope::Scoped(i.to_string().into());
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
        let deadline = tokio::time::Instant::from_std(start + duration);
        let symbolication = Arc::clone(&symbolication);
        let workload = Arc::clone(&workload);

        let task = tokio::spawn(async move {
            let task_durations = Arc::new(Mutex::new(DDSketch::default()));
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
                        let task_durations = Arc::clone(&task_durations);
                        let task_start = Instant::now();

                        let hub = sentry::Hub::new_from_top(sentry::Hub::current());
                        let ctx = sentry::TransactionContext::new("stresstest", "stresstest");
                        let transaction = hub.start_transaction(ctx);

                        let future = async move {
                            process_payload(&symbolication, &workload).await;

                            transaction.finish();

                            task_durations.lock().unwrap().add(task_start.elapsed().as_secs_f64());

                            drop(permit);
                        };
                        let future = future.bind_hub(hub);

                        tokio::spawn(future);
                    }
                    _ = &mut sleep => {
                        break;
                    }
                }
            }

            let task_durations: DDSketch = {
                let mut task_durations = task_durations.lock().unwrap();
                std::mem::take(&mut task_durations)
            };

            // by acquiring *all* the semaphores, we essentially wait for all outstanding tasks to finish
            let _permits = semaphore.acquire_many(concurrency as u32).await;

            (concurrency, task_durations)
        });
        tasks.push(task);
    }

    let finished_tasks = futures::future::join_all(tasks).await;

    for (i, task) in finished_tasks.into_iter().enumerate() {
        let (concurrency, task_durations) = task.unwrap();

        let ops = task_durations.count();
        let ops_ps = ops as f32 / duration.as_secs() as f32;
        println!("Workload {i} (concurrency: {concurrency}): {ops} operations, {ops_ps:.2} ops/s");

        let avg = Duration::from_secs_f64(task_durations.sum().unwrap() / ops as f64);
        let p50 = Duration::from_secs_f64(task_durations.quantile(0.5).unwrap().unwrap());
        let p90 = Duration::from_secs_f64(task_durations.quantile(0.9).unwrap().unwrap());
        let p99 = Duration::from_secs_f64(task_durations.quantile(0.99).unwrap().unwrap());
        println!("  avg: {avg:.2?}; p50: {p50:.2?}; p90: {p90:.2?}; p99: {p99:.2?}");
    }

    Ok(())
}
