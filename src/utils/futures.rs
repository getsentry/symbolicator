use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::compat::Future01CompatExt;
use futures::{future, FutureExt, TryFutureExt};
use futures01::future::Future as Future01;
use tokio01::prelude::FutureExt as TokioFutureExt;
use tokio01::runtime::Runtime as TokioRuntime;
use tokio_retry::strategy::{jitter, ExponentialBackoff};

static IS_TEST: AtomicBool = AtomicBool::new(false);

/// A pinned, boxed future.
///
/// This is the type of future that [`futures::FutureExt::boxed`] would return.  This is
/// pretty boring but clippy quickly complains about type complexity without this.
///
/// You would mainly use this if you are dealing with a trait methods which deals with
/// futures.  Trait methods can not be async/await and using this type in their return value
/// allows to integrate with async await code.
pub type BoxedFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// Enables test mode of all thread pools and remote threads.
///
/// In this mode, futures are not spawned into threads, but instead run on the current thread. This
/// is useful to ensure deterministic test execution, and also allows to capture console output from
/// spawned tasks.
#[cfg(test)]
pub fn enable_test_mode() {
    IS_TEST.store(true, Ordering::Relaxed);
}

/// Error returned from a [`SpawnHandle`] when the thread pool restarts.
pub use oneshot::Canceled as RemoteCanceled;

/// Handle returned from [`ThreadPool::spawn_handle`].
///
/// This handle is a future representing the completion of a different future spawned on to the
/// thread pool. Created through the [`ThreadPool::spawn_handle`] function this handle will resolve
/// when the future provided resolves on the thread pool. If the remote thread restarts due to
/// panics, [`SpawnError::Canceled`] is returned.
pub use oneshot::Receiver as SpawnHandle;

/// Work-stealing based thread pool for executing futures.
#[derive(Clone, Debug)]
pub struct ThreadPool {
    inner: Option<Arc<TokioRuntime>>,
}

impl ThreadPool {
    /// Create a new `ThreadPool`.
    ///
    /// Since we create a CPU-heavy and an IO-heavy pool we reduce the
    /// number of threads used per pool so that the pools are less
    /// likely to starve each other.
    pub fn new() -> Self {
        let inner = if cfg!(test) && IS_TEST.load(Ordering::Relaxed) {
            None
        } else {
            let runtime = tokio01::runtime::Builder::new().build().unwrap();
            Some(Arc::new(runtime))
        };

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

        let compat = spawned.boxed().compat();

        match self.inner {
            Some(ref runtime) => runtime.executor().spawn(compat),
            None => actix::spawn(compat),
        }

        receiver
    }
}

/// A remote thread which can run non-sendable futures.
///
/// This can be used to implement any services which internally need to use
/// non-`Send`able futures and are sufficient with running on single thread.
///
/// When the global `IS_TEST` is set by calling `enable_test_mode`, this will
/// not create a new thread and instead spawn any executions in the current
/// thread.  This is a workaround for the stdout and panic capturing not being
/// exposed properly to the test framework users which stops us from enabling
/// these things in spawned threads during tests.
#[derive(Clone, Debug)]
pub struct RemoteThread {
    inner: Arc<InnerRemoteThread>,
}

/// Inner [`RemoteThread`] struct to get correct clone behaviour.
///
/// We only have an [`actix::Addr`] to the arbiter, which itself can be cloned. Dropping this
/// does not terminate the arbiter, but we wan the [`RemoteThread`] to stop when dropped. So
/// we need to implement [`Drop`], but only want to actually trigger this drop when all clones
/// of our [`RemoteThread`] are dropped.  Hence the need to put the drop on an inner struct
/// which is wrapped in an Arc.
#[derive(Debug)]
struct InnerRemoteThread {
    arbiter: Option<actix::Addr<actix::Arbiter>>,
}

/// Spawning the future on the remote failed.
///
/// This is an error caused by the remote, not the result or output of the future itself.
/// In the future this could include loadshedding errors and the like.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SpawnError {
    /// The future was cancelled, e.g. because the remote was dropped.
    Canceled,
    /// The future exceeded its timeout.
    Timeout,
}

/// Dropping terminates the [`RemoteThread`], cancelling all futures.
impl Drop for InnerRemoteThread {
    fn drop(&mut self) {
        // Without sending the StopArbiter message the arbiter thread keeps running even if
        // you drop the arbiter.
        if let Some(ref addr) = self.arbiter {
            addr.do_send(actix::msgs::StopArbiter(0));
        }
    }
}

impl RemoteThread {
    /// Create a new instance, spawning the thread.
    pub fn new() -> Self {
        let arbiter = if cfg!(test) && IS_TEST.load(Ordering::Relaxed) {
            None
        } else {
            Some(actix::Arbiter::new("RemoteThread"))
        };
        Self {
            inner: Arc::new(InnerRemoteThread { arbiter }),
        }
    }

    /// Create a new instance, even when running in test mode.
    ///
    /// Some tests actually need to test this stuff works, support that.
    #[cfg(test)]
    pub fn new_threaded() -> Self {
        Self {
            inner: Arc::new(InnerRemoteThread {
                arbiter: Some(actix::Arbiter::new("RemoteThread")),
            }),
        }
    }

    /// Create a future in the remote thread and run it.
    ///
    /// The returned future resolves when the spawned future has completed execution.  The
    /// output type of the spawned future is wrapped in a `Result`, normally the output is
    /// returned in an `Ok`.
    ///
    /// When the future takes too long [`SpawnError::Timeout`] is returned, if the
    /// `RemoteThread` it is running on is dropped [`SpawnError::Canceled`] is returned.
    pub fn spawn<F, R, T>(
        &self,
        task_name: &'static str,
        timeout: Duration,
        factory: F,
    ) -> impl Future<Output = Result<T, SpawnError>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        enum ChannelMsg<T> {
            Output(T),
            Timeout,
        }
        let (tx, rx) = oneshot::channel();
        let creation_time = Instant::now();
        let msg = actix::msgs::Execute::new(move || -> Result<(), ()> {
            let fut01 = futures01::future::lazy(move || {
                metric!(
                    timer("futures.wait_time") = creation_time.elapsed(),
                    "task_name" => task_name,
                );
                let start_time = Instant::now();
                factory()
                    .unit_error()
                    .boxed_local()
                    .compat()
                    .timeout(timeout)
                    .then(move |r: Result<T, tokio01::timer::timeout::Error<()>>| {
                        metric!(
                            timer("futures.done") = start_time.elapsed(),
                            "task_name" => task_name,
                            "status" => if r.is_ok() {"done"} else {"timeout"},
                        );
                        let msg = match r {
                            Ok(o) => ChannelMsg::Output(o),
                            Err(_) => {
                                // Because we wrapped our T into Ok() above using
                                // .unit_error() to make a TryFuture, we know the <Timeout
                                // as Future>::Error will only occur if the error is
                                // actually because of a timeout, so we do not need to check
                                // with .is_timer() or .is_inner().
                                ChannelMsg::Timeout
                            }
                        };
                        tx.send(msg).unwrap_or_else(|_| {
                            log::debug!(
                                "Failed to send result of {} task, caller dropped",
                                task_name
                            )
                        });
                        futures01::future::ok(())
                    })
            });
            actix::spawn(fut01);
            Ok(())
        });
        match self.inner.arbiter {
            Some(ref arbiter) => arbiter.do_send(msg),
            None => {
                let arbiter = actix::Arbiter::current();
                arbiter.do_send(msg);
            }
        }
        rx.then(move |oneshot_result| match oneshot_result {
            Ok(ChannelMsg::Output(o)) => future::ready(Ok(o)),
            Ok(ChannelMsg::Timeout) => future::ready(Err(SpawnError::Timeout)),
            Err(oneshot::Canceled) => {
                metric!(
                    counter("futures.canceled") += 1,
                    "task_name" => task_name,
                );
                future::ready(Err(SpawnError::Canceled))
            }
        })
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
    pub fn timed<T, TE>(result: &Result<T, TE>) -> &'static str {
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

/// Retry a future 3 times with exponential backoff.
pub async fn retry<G, F, T, E>(task_gen: G) -> Result<T, E>
where
    G: Fn() -> F,
    F: Future<Output = Result<T, E>>,
{
    let mut backoff = ExponentialBackoff::from_millis(10).map(jitter).take(3);
    loop {
        let result = task_gen().await;
        match backoff.next() {
            Some(duration) if result.is_err() => delay(duration).await,
            _ => break result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test;

    /// Elaborate executor-neutral way of sleeping in a future
    async fn sleep(duration: Duration) {
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            tx.send(()).ok();
        });
        rx.await.ok();
    }

    /// Create a new RemoteThread for testing purposes.
    fn setup_remote_thread() -> RemoteThread {
        // Ensure actix is setup.
        test::setup();

        // test::setup() enables logging, but this test spawns a thread where
        // logging is not captured.  For normal test runs we don't want to
        // pollute the stdout so silence logs here.  When debugging this test
        // you may want to temporarily remove this.
        log::set_max_level(log::LevelFilter::Off);

        RemoteThread::new_threaded()
    }

    #[test]
    fn test_remote_thread() {
        let remote = setup_remote_thread();
        let fut = remote.spawn("task", Duration::from_secs(10), || future::ready(42));
        let ret = test::block_fn(|| fut);
        assert_eq!(ret, Ok(42));
    }

    #[test]
    fn test_remote_thread_timeout() {
        let remote = setup_remote_thread();
        let fut = remote.spawn("task", Duration::from_nanos(1), || {
            sleep(Duration::from_secs(1))
        });
        let ret = test::block_fn(|| fut);
        assert_eq!(ret, Err(SpawnError::Timeout));
    }

    #[test]
    fn test_remote_thread_canceled() {
        let remote = setup_remote_thread();
        let fut = remote.spawn("task", Duration::from_secs(2), || {
            sleep(Duration::from_secs(1))
        });
        std::mem::drop(remote);
        let ret = test::block_fn(|| fut);
        assert_eq!(ret, Err(SpawnError::Canceled));
    }

    #[test]
    fn test_remote_thread_clone_drop() {
        // When dropping a clone of the RemoteThread the other one needs to keep working.
        let remote0 = setup_remote_thread();
        let remote1 = remote0.clone();
        std::mem::drop(remote0);
        let fut = remote1.spawn("task", Duration::from_secs(1), || future::ready(42));
        let ret = test::block_fn(|| fut);
        assert_eq!(ret, Ok(42));
    }

    #[test]
    fn test_timeout_compat() {
        test::block_fn(|| async {
            let long_job = sleep(Duration::from_secs(1));
            let result: Result<(), Elapsed> =
                timeout_compat(Duration::from_millis(10), long_job).await;
            assert_eq!(result, Err(Elapsed(())));
        });

        test::block_fn(|| async {
            let long_job = sleep(Duration::from_millis(10));
            let result: Result<(), Elapsed> =
                timeout_compat(Duration::from_secs(1), long_job).await;
            assert_eq!(result, Ok(()));
        });
    }
}
