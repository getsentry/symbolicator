use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::{compat::Future01CompatExt, select, FutureExt, TryFutureExt};
use tokio::runtime::Runtime as TokioRuntime;
use tokio::timer::Delay as TokioDelay;

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
pub use oneshot::Canceled;

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
    /// Create a new `ThreadPool` with default values.
    pub fn new() -> Self {
        let inner = if cfg!(test) && IS_TEST.load(Ordering::Relaxed) {
            None
        } else {
            Some(Arc::new(TokioRuntime::new().unwrap()))
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

pub struct Elapsed;

pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, Elapsed>
where
    F: Future,
{
    let delay = TokioDelay::new(Instant::now() + duration);

    select! {
        output = future.fuse() => Ok(output),
        _ = delay.compat().fuse() => Err(Elapsed),
    }
}

pub async fn timeout_with<F, T, E, R, O>(duration: Duration, future: F, or_else: O) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    O: FnOnce() -> R,
    R: Into<E>,
{
    match timeout(duration, future).await {
        Ok(result) => result,
        Err(_) => Err(or_else().into()),
    }
}
