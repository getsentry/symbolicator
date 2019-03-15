use std::time::{Duration, Instant};

use futures::{future::Either, lazy, Future};

use tokio::prelude::FutureExt;

pub fn measure_task<T, E, F>(
    task_name: &'static str,
    timeout: Option<(Duration, F)>,
    fut: impl Future<Item = T, Error = E>,
) -> impl Future<Item = T, Error = E>
where
    F: FnOnce() -> E,
{
    let creation_time = Instant::now();

    lazy(move || {
        metric!(timer(&format!("{}.wait_time", task_name)) = creation_time.elapsed());
        let start_time = Instant::now();

        let fut = if let Some((timeout, timeout_e)) = timeout {
            Either::A(fut.timeout(timeout).map_err(move |e| {
                e.into_inner().unwrap_or_else(|| {
                    metric!(counter(&format!("{}.timeout", task_name)) += 1);
                    timeout_e()
                })
            }))
        } else {
            Either::B(fut)
        };

        fut.then(move |result| {
            metric!(timer(&format!("{}.processing_time", task_name)) = start_time.elapsed());
            match result {
                Ok(_) => metric!(counter(&format!("{}.success", task_name)) += 1),
                Err(_) => metric!(counter(&format!("{}.failure", task_name)) += 1),
            };
            result
        })
    })
}
