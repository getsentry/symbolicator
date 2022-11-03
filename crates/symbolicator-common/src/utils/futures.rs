use std::borrow::Cow;
use std::future::Future;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::metrics::{self, prelude::*};

/// Execute a callback on dropping of the container type.
///
/// The callback must not panic under any circumstance. Since it is called while dropping an item,
/// this might result in aborting program execution.
pub struct CallOnDrop {
    f: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl CallOnDrop {
    /// Creates a new `CallOnDrop`.
    pub fn new<F: FnOnce() + Send + 'static>(f: F) -> CallOnDrop {
        CallOnDrop {
            f: Some(Box::new(f)),
        }
    }
}

impl Drop for CallOnDrop {
    fn drop(&mut self) {
        if let Some(f) = self.f.take() {
            f();
        }
    }
}

/// Cancels the [`JoinHandle`] on drop.
///
/// Spawning a task on a runtime means it will run independently from the code that calls `spawn`,
/// even if the code stops polling the [`JoinHandle`]. We have various timeouts configured throughout
/// the codebase, and some of those are attached to [`JoinHandle`]s.
/// Which means we stop polling the handle, but the task that was spawned will continue on.
///
/// This type makes sure that the spawned task is being canceled/aborted in case we lose interest
/// in it.
#[must_use = "this will cancel the underlying task when dropped"]
pub struct CancelOnDrop<T> {
    handle: JoinHandle<T>,
}

impl<T> CancelOnDrop<T> {
    pub fn new(handle: JoinHandle<T>) -> Self {
        Self { handle }
    }
}

impl<T> Drop for CancelOnDrop<T> {
    fn drop(&mut self) {
        self.handle.abort()
    }
}

impl<T> Future for CancelOnDrop<T> {
    type Output = <JoinHandle<T> as Future>::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // https://doc.rust-lang.org/std/pin/index.html#pinning-is-structural-for-field
        let handle = unsafe { self.map_unchecked_mut(|s| &mut s.handle) };
        handle.poll(cx)
    }
}

/// State of the [`MeasureGuard`].
#[derive(Clone, Copy, Debug)]
enum MeasureState {
    /// The future is not ready.
    Pending,
    /// The future has terminated with a status.
    Done(&'static str),
}

/// A guard to [`measure`] the execution of futures.
struct MeasureGuard<'a> {
    state: MeasureState,
    task_name: &'a str,
    tag: Option<(&'a str, Cow<'a, str>)>,
    creation_time: Instant,
}

impl<'a> MeasureGuard<'a> {
    /// Creates a new measure guard.
    pub fn new(task_name: &'a str, tag: Option<(&'a str, Cow<'a, str>)>) -> Self {
        Self {
            state: MeasureState::Pending,
            task_name,
            tag,
            creation_time: Instant::now(),
        }
    }

    /// Marks the future as started.
    ///
    /// By default, the future is waiting to be polled. `start` emits the `futures.wait_time`
    /// metric.
    pub fn start(&mut self) {
        metrics::with_client(|client| {
            let mut metric = client
                .time_with_tags("futures.wait_time", self.creation_time.elapsed())
                .with_tag("task_name", self.task_name);
            if let Some((k, v)) = &self.tag {
                metric = metric.with_tag(k, v.as_ref());
            }

            client.send_metric(metric);
        })
    }

    /// Marks the future as terminated and emits the `futures.done` metric.
    pub fn done(mut self, status: &'static str) {
        self.state = MeasureState::Done(status);
    }
}

impl Drop for MeasureGuard<'_> {
    fn drop(&mut self) {
        let status = match self.state {
            MeasureState::Pending => "canceled",
            MeasureState::Done(status) => status,
        };
        metrics::with_client(|client| {
            let mut metric = client
                .time_with_tags("futures.done", self.creation_time.elapsed())
                .with_tag("task_name", self.task_name)
                .with_tag("status", status);
            if let Some((k, v)) = &self.tag {
                metric = metric.with_tag(k, v.as_ref());
            }

            client.send_metric(metric);
        })
    }
}

/// Measures the timing of a future and reports metrics.
///
/// This function reports two metrics:
///
///  - `futures.wait_time`: Time between creation of the future and the first poll.
///  - `futures.done`: Time between creation of the future and completion.
///
/// The metric is tagged with a status derived with the `get_status` function. See the [`m`] module
/// for status helpers.
pub fn measure<'a, S, F>(
    task_name: &'a str,
    get_status: S,
    tag: Option<(&'a str, Cow<'a, str>)>,
    f: F,
) -> impl Future<Output = F::Output> + 'a
where
    F: 'a + Future,
    S: 'a + FnOnce(&F::Output) -> &'static str,
{
    let mut guard = MeasureGuard::new(task_name, tag);

    async move {
        guard.start();
        let output = f.await;
        guard.done(get_status(&output));
        output
    }
}

/// Status helpers for [`measure`].
#[allow(dead_code)]
pub mod m {
    /// Creates an `"ok"` status for [`measure`](super::measure).
    pub fn ok<T>(_t: &T) -> &'static str {
        "ok"
    }

    /// Creates a status derived from the future's result for [`measure`](super::measure).
    ///
    ///  - `"ok"` if the future resolves to `Ok(_)`
    ///  - `"err"` if the future resolves to `Err(_)`
    pub fn result<T, E>(result: &Result<T, E>) -> &'static str {
        match result {
            Ok(_) => "ok",
            Err(_) => "err",
        }
    }

    /// Creates a status derived from the future's result for [`measure`](super::measure).
    ///
    ///  - `"ok"` if the future resolves to `Ok(_)`
    ///  - `"timeout"` if the future times out
    pub fn timed<T>(result: &Result<T, tokio::time::error::Elapsed>) -> &'static str {
        match result {
            Ok(_) => "ok",
            Err(_) => "timeout",
        }
    }

    /// Creates a status derived from the future's result for [`measure`](super::measure).
    ///
    ///  - `"ok"` if the future resolves to `Ok(_)`
    ///  - `"err"` if the future resolves to `Err(_)
    ///  - `"timeout"` if the future times out
    pub fn timed_result<T, E, TE>(result: &Result<Result<T, E>, TE>) -> &'static str {
        // TODO: `TE` should be `tokio::time::error::Elapsed`, but since we have to deal with
        // multiple versions of the timer, we assume that this is never called on other nested
        // results.
        match result {
            Ok(inner) => self::result(inner),
            Err(_) => "timeout",
        }
    }
}

/// Retry a future 3 times with 20 millisecond delays.
pub async fn retry<G, F, T, E>(mut task_gen: G) -> Result<T, E>
where
    G: FnMut() -> F,
    F: Future<Output = Result<T, E>>,
{
    let mut retries = 0;
    loop {
        let result = task_gen().await;
        if result.is_ok() || retries >= 3 {
            break result;
        }

        retries += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
