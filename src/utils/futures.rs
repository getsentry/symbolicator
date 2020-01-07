use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures03::channel::oneshot;
use futures03::{FutureExt, TryFutureExt};
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

pub use oneshot::Canceled;

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

    pub fn spawn_handle<F>(&self, future: F) -> impl Future<Output = Result<F::Output, Canceled>>
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
