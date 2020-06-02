use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::channel::oneshot;
use futures::{FutureExt, TryFutureExt};
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

/// Error returned from a `SpawnHandle` when the thread pool restarts.
pub use oneshot::Canceled as RemoteCanceled;

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
/// [`enable_test_mode`]: func.enable_test_mode.html
#[derive(Clone, Debug)]
pub struct RemoteThread {
    arbiter: Option<actix::Addr<actix::Arbiter>>,
}

impl RemoteThread {
    /// Create a new instance, spawning the thread.
    pub fn new() -> Self {
        let arbiter = if cfg!(test) && IS_TEST.load(Ordering::Relaxed) {
            None
        } else {
            Some(actix::Arbiter::new("RemoteThread"))
        };
        Self { arbiter }
    }

    /// Create a new instance, even when running in test mode.
    ///
    /// Some tests actually need to test this stuff works, support that.
    #[cfg(test)]
    pub fn new_threaded() -> Self {
        Self {
            arbiter: Some(actix::Arbiter::new("RemoteThread")),
        }
    }

    /// Create a future in the remote thread and run it.
    ///
    /// The returned future resolves when the spawned future has completed
    /// execution.  The output type of the spawned future is wrapped in a
    /// `Result`, normally the output is returned in an `Ok` but when the remote
    /// future is dropped because the thread is shut down or restarted for any
    /// reason it returns `Err(oneshot::Canceled)`.
    pub fn spawn<F, R, T>(&self, factory: F) -> impl Future<Output = Result<T, RemoteCanceled>>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let msg = actix::msgs::Execute::new(|| -> Result<(), ()> {
            let fut = factory().map(|output| tx.send(output).or(Err(())));
            let fut01 = fut.boxed_local().compat();
            actix::spawn(fut01);
            Ok(())
        });
        match self.arbiter {
            Some(ref arbiter) => arbiter.do_send(msg),
            None => {
                let arbiter = actix::Arbiter::current();
                arbiter.do_send(msg);
            }
        }
        rx
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
