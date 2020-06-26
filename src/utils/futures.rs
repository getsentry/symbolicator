use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::compat::Future01CompatExt;
use futures::{FutureExt, TryFutureExt};
use tokio::prelude::FutureExt as TokioFutureExt;
use tokio::runtime::Runtime as TokioRuntime;

static IS_TEST: AtomicBool = AtomicBool::new(false);

/// Enables test mode of all thread pools and remote threads.
///
/// In this mode, futures are not spawned into threads, but instead run on the current thread. This
/// is useful to ensure deterministic test execution, and also allows to capture console output from
/// spawned tasks.
#[cfg(test)]
pub fn enable_test_mode() {
    IS_TEST.store(true, Ordering::Relaxed);
}

/// Error returned when the spawned future was canceled.
use oneshot::Canceled as RemoteCanceled;

/// Handle returned from `ThreadPool::spawn_handle`.
///
/// This handle is a future representing the completion of a different future spawned on to the
/// thread pool. Created through the `ThreadPool::spawn_handle` function this handle will resolve
/// when the future provided resolves on the thread pool. If the remote thread restarts due to
/// panics, `SpawnError::Canceled` is returned.
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
            let runtime = tokio::runtime::Builder::new().build().unwrap();
            Some(Arc::new(runtime))
        };

        ThreadPool { inner }
    }

    /// Spawn a future on to the thread pool, return a future representing the produced value.
    ///
    /// The `SpawnHandle` returned is a future that is a proxy for future itself. When future
    /// completes on this thread pool then the SpawnHandle will itself be resolved.
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

/// A plain remote thread which can execute non-send futures.
///
/// When the global `IS_TEST` is set by calling [`enable_test_mode`] this will
/// not create a new thread and instead spawn any executions in the current
/// thread.  This is a workaround for the stdout and panic capturing not being
/// exposed properly to the test framework users which stops us from enabling
/// these things in spawned threads during tests.
///
/// [`enable_test_mode`]: func.enable_test_mode.html
#[derive(Debug, Clone)]
struct BareRemoteThread {
    inner: Arc<InnerBareRemoteThread>,
}

/// Inner `BareRemoteThread` struct to get correct clone behaviour.
///
/// We only have an `actix::Addr` to the arbiter, which itself can be cloned.  Dropping this
/// does not terminate the arbiter, but we wan the `RemoteThread` to stop when dropped.  So
/// we need to implement `Drop`, but only want to actually trigger this drop when all clones
/// of our `RemoteThread` are dropped.  Hence the need to put the drop on an inner struct
/// which is wrapped in an Arc.
#[derive(Debug)]
struct InnerBareRemoteThread {
    arbiter: Option<actix::Addr<actix::Arbiter>>,
}

/// Dropping terminates the BareRemoteThread, cancelling all futures.
impl Drop for InnerBareRemoteThread {
    fn drop(&mut self) {
        // Without sending the StopArbiter message the arbiter thread keeps running even if
        // you drop the arbiter.
        if let Some(ref addr) = self.arbiter {
            addr.do_send(actix::msgs::StopArbiter(0));
        }
    }
}

impl BareRemoteThread {
    /// Create a new instance, spawning the thread.
    fn new() -> Self {
        let arbiter = if cfg!(test) && IS_TEST.load(Ordering::Relaxed) {
            None
        } else {
            Some(actix::Arbiter::new("RemoteThread"))
        };
        Self {
            inner: Arc::new(InnerBareRemoteThread { arbiter }),
        }
    }

    /// Create a new instance, even when running in test mode.
    ///
    /// Some tests actually need to test this stuff works, support that.
    #[cfg(test)]
    pub fn new_threaded() -> Self {
        Self {
            inner: Arc::new(InnerBareRemoteThread {
                arbiter: Some(actix::Arbiter::new("RemoteThread")),
            }),
        }
    }

    /// Create a future in the remote thread and run it.
    ///
    /// The returned future resolves when the spawned future has completed execution.  The
    /// output type of the spawned future is wrapped in a `Result`, normally the output is
    /// returned in an `Ok`.
    // TODO: Dropping the returned future should cancel the spawned future.
    pub fn spawn<F, R, T>(&self, factory: F) -> impl Future<Output = Result<T, RemoteCanceled>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let msg = actix::msgs::Execute::new(move || -> Result<(), ()> {
            let fut = factory().map(|output| tx.send(output).or(Err(())));
            let fut01 = fut.boxed_local().compat();
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
        rx
    }
}

/// A remote thread which can run non-sendable futures.
///
/// This can be used to implement any services which internally need to use
/// non-`Send`able futures and are sufficient with running on single thread.
///
/// When the global `IS_TEST` is set by calling [`enable_test_mode`] this will
/// not create a new thread and instead spawn any executions in the current
/// thread.  This is a workaround for the stdout and panic capturing not being
/// exposed properly to the test framework users which stops us from enabling
/// these things in spawned threads during tests.
///
/// NOTE: This implementation will still change to not handle the timeout and instead
///    support cancellation on dropping of the returned future.
///
/// [`enable_test_mode`]: func.enable_test_mode.html
#[derive(Clone, Debug)]
pub struct RemoteThread {
    remote: BareRemoteThread,
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

impl RemoteThread {
    /// Create a new instance, spawning the thread.
    pub fn new() -> Self {
        Self {
            remote: BareRemoteThread::new(),
        }
    }

    /// Create a new instance, even when running in test mode.
    ///
    /// Some tests actually need to test this stuff works, support that.
    #[cfg(test)]
    pub fn new_threaded() -> Self {
        Self {
            remote: BareRemoteThread::new_threaded(),
        }
    }

    /// Create a future in the remote thread and run it.
    ///
    /// The returned future resolves when the spawned future has completed execution.  The
    /// output type of the spawned future is wrapped in a `Result`, normally the output is
    /// returned in an `Ok`.
    ///
    /// When the future takes too long `SpawnError::Timeout` is returned, if the
    /// `RemoteThread` it is running on is dropped `SpawnError::Canceled` is returned.
    // TODO: rename to spawn_task?
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
        };
        let creation_time = Instant::now();
        let spawn_rx = self.remote.spawn(move || async move {
            metric!(
                timer("futures.wait_time") = creation_time.elapsed(),
                "task_name" => task_name,
            );
            let start_time = Instant::now();
            let timeout_output = factory()
                .unit_error()
                .boxed_local()
                .compat()
                .timeout(timeout)
                .compat()
                .await;
            metric!(
                timer("futures.done") = start_time.elapsed(),
                "task_name" => task_name,
                "status" => if timeout_output.is_ok() {"done"} else {"timeout"},
            );
            match timeout_output {
                Ok(output) => ChannelMsg::Output(output),
                // Because we wrapped the original future in Ok with .unit_error() we know
                // any error here is from the timeout and not from the future.  So we do not
                // need to check using with .is_timer() or .is_inner().
                Err(_) => ChannelMsg::Timeout,
            }
        });
        async move {
            match spawn_rx.await {
                Ok(ChannelMsg::Output(output)) => Ok(output),
                Ok(ChannelMsg::Timeout) => Err(SpawnError::Timeout),
                Err(RemoteCanceled) => {
                    metric!(
                        counter("futures.canceled") += 1,
                        "task_name" => task_name,
                    );
                    Err(SpawnError::Canceled)
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    use futures::future;

    use crate::test;

    fn setup() {
        // Ensure actix is setup.
        test::setup();

        // test::setup() enables logging, but this test spawns a thread where
        // logging is not captured.  For normal test runs we don't want to
        // pollute the stdout so silence logs here.  When debugging this test
        // you may want to temporarily remove this.
        log::set_max_level(log::LevelFilter::Off);
    }

    #[test]
    fn test_bare_remote_thread() {
        setup();
        let remote = BareRemoteThread::new_threaded();
        let fut = remote.spawn(|| future::ready(42));
        let ret = test::block_fn03(|| fut);
        assert_eq!(ret, Ok(42));
    }

    #[test]
    fn test_bare_remote_thread_canceled() {
        setup();
        let remote = BareRemoteThread::new_threaded();
        let fut = remote.spawn(|| {
            // Elaborate executor-neutral way of sleeping in a future.
            let (tx, rx) = oneshot::channel();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(10));
                tx.send(42).ok();
            });
            rx
        });
        std::mem::drop(remote);
        let ret = test::block_fn03(|| fut);
        assert_eq!(ret, Err(RemoteCanceled));
    }

    #[test]
    fn test_bare_remote_thread_clone_drop() {
        // When dropping a clone of the BareRemoteThread the other one needs to keep working.
        setup();
        let remote0 = BareRemoteThread::new_threaded();
        let remote1 = remote0.clone();
        std::mem::drop(remote0);
        let fut = remote1.spawn(|| future::ready(42));
        let ret = test::block_on(fut);
        assert_eq!(ret, Ok(42));
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
        let ret = test::block_on(fut);
        assert_eq!(ret, Ok(42));
    }

    #[test]
    fn test_remote_thread_timeout() {
        let remote = setup_remote_thread();
        let fut = remote.spawn("task", Duration::from_nanos(1), || {
            // Elaborate executor-neutral way of sleeping in a future
            let (tx, rx) = oneshot::channel();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(1));
                tx.send(42).ok();
            });
            rx
        });
        let ret = test::block_on(fut);
        assert_eq!(ret, Err(SpawnError::Timeout));
    }

    #[test]
    fn test_remote_thread_canceled() {
        let remote = setup_remote_thread();
        let fut = remote.spawn("task", Duration::from_secs(10), || {
            // Elaborate executor-neutral way of sleeping in a future
            let (tx, rx) = oneshot::channel();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(10));
                tx.send(42).ok();
            });
            rx
        });
        std::mem::drop(remote);
        let ret = test::block_on(fut);
        assert_eq!(ret, Err(SpawnError::Canceled));
    }

    #[test]
    fn test_remote_thread_clone_drop() {
        // When dropping a clone of the RemoteThread the other one needs to keep working.
        let remote0 = setup_remote_thread();
        let remote1 = remote0.clone();
        std::mem::drop(remote0);
        let fut = remote1.spawn("task", Duration::from_secs(10), || future::ready(42));
        let ret = test::block_on(fut);
        assert_eq!(ret, Ok(42));
    }
}
