use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::compat::Future01CompatExt;
use futures::{FutureExt, TryFutureExt};
use tokio01::prelude::FutureExt as TokioFutureExt;
use tokio01::runtime::Runtime as TokioRuntime;

/// A pinned, boxed future.
///
/// This is the type of future that [`futures::FutureExt::boxed`] would return.  This is
/// pretty boring but clippy quickly complains about type complexity without this.
///
/// You would mainly use this if you are dealing with a trait methods which deals with
/// futures.  Trait methods can not be async/await and using this type in their return value
/// allows to integrate with async await code.
pub type BoxedFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// Error returned from a [`SpawnHandle`] when the thread pool restarts.
pub use oneshot::Canceled as RemoteCanceled;

/// Handle returned from [`ThreadPool::spawn_handle`].
///
/// This handle is a future representing the completion of a different future spawned on to the
/// thread pool. Created through the [`ThreadPool::spawn_handle`] function this handle will resolve
/// when the future provided resolves on the thread pool.
pub use oneshot::Receiver as SpawnHandle;

/// Work-stealing based thread pool for executing futures.
#[derive(Clone, Debug)]
pub struct ThreadPool {
    inner: Arc<TokioRuntime>,
}

impl ThreadPool {
    /// Create a new `ThreadPool`.
    ///
    /// Since we create a CPU-heavy and an IO-heavy pool we reduce the
    /// number of threads used per pool so that the pools are less
    /// likely to starve each other.
    pub fn new() -> Self {
        let inner = Arc::new(tokio01::runtime::Builder::new().build().unwrap());
        ThreadPool { inner }
    }

    /// Spawn a future on to the thread pool, return a future representing the produced value.
    ///
    /// The [`SpawnHandle`] returned is a future that is a proxy for future itself. When
    /// future completes on this thread pool then the SpawnHandle will itself be resolved
    /// and the outcome of the spawned future will be in the `Ok` variant.  If the spawned
    /// future got cancelled the outcome of this proxy future will resolve into an `Err`
    /// variant.
    pub fn spawn_handle<F>(&self, future: F) -> SpawnHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();

        let spawned = async move {
            sender.send(future.await).ok();
            Ok(())
        };

        self.inner.executor().spawn(spawned.boxed().compat());

        receiver
    }
}

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

/// Executes a future on the current thread.
///
/// The provided future must complete or be canceled before `run` will return. This function will
/// always spawn on a `CurrentThread` executor and is able to spawn futures that are not `Send`.
///
/// # Panics
///
/// This function can only be invoked from the context of a `run` call; any other use will result in
/// a panic.
///
/// # Compatibility
///
/// This is a compatibility shim for the `tokio` 1.0 `spawn` function signature to run on a `tokio`
/// 0.1 executor.
pub fn spawn_compat<F>(future: F)
where
    F: Future + 'static,
{
    tokio01::runtime::current_thread::spawn(future.map(|_| Ok(())).boxed_local().compat());
}

/// Error returned by [`timeout_compat`].
#[derive(Debug, PartialEq, thiserror::Error)]
#[error("deadline has elapsed")]
pub struct Elapsed(());

/// Require a `Future` to complete before the specified duration has elapsed.
///
/// If the future completes before the duration has elapsed, then the completed value is returned.
/// Otherwise, an error is returned and the future is canceled.
///
/// # Compatibility
///
/// This is a compatibility shim for the `tokio` 1.0 `timeout` function signature to run on a
/// `tokio` 0.1 executor.
pub async fn timeout_compat<F: Future>(duration: Duration, f: F) -> Result<F::Output, Elapsed> {
    f.unit_error()
        .boxed_local()
        .compat()
        .timeout(duration)
        .compat()
        .await
        .map_err(|_| Elapsed(()))
}

/// Delay aka sleep for a given duration
pub async fn delay(duration: Duration) {
    tokio01::timer::Delay::new(Instant::now() + duration)
        .compat()
        .await
        .ok();
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
    creation_time: Instant,
}

impl<'a> MeasureGuard<'a> {
    /// Creates a new measure guard.
    pub fn new(task_name: &'a str) -> Self {
        Self {
            state: MeasureState::Pending,
            task_name,
            creation_time: Instant::now(),
        }
    }

    /// Marks the future as started.
    ///
    /// By default, the future is waiting to be polled. `start` emits the `futures.wait_time`
    /// metric.
    pub fn start(&mut self) {
        metric!(
            timer("futures.wait_time") = self.creation_time.elapsed(),
            "task_name" => self.task_name,
        );
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

        metric!(
            timer("futures.done") = self.creation_time.elapsed(),
            "task_name" => self.task_name,
            "status" => status,
        );
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
    f: F,
) -> impl Future<Output = F::Output> + 'a
where
    F: 'a + Future,
    S: 'a + FnOnce(&F::Output) -> &'static str,
{
    let mut guard = MeasureGuard::new(task_name);

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
        delay(Duration::from_millis(20)).await;
    }
}
