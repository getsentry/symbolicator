use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::{sync::oneshot, Async, Future, Poll};
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

/// Error of a spawned future.
///
/// The error can either be an error of the inner future, or `Canceled` if execution of the future
/// was canceled before it completed. This happens when the remote executor has to restart due to a
/// panic or crash.
#[derive(Clone, Copy, Debug)]
pub enum SpawnError<E> {
    Error(E),
    Canceled,
}

impl<E> SpawnError<E> {
    pub fn map_canceled<F, R>(self, f: F) -> E
    where
        F: FnOnce() -> R,
        R: Into<E>,
    {
        match self {
            Self::Error(e) => e,
            Self::Canceled => f().into(),
        }
    }
}

/// Handle returned from `ThreadPool::spawn_handle`.
///
/// This handle is a future representing the completion of a different future spawned on to the
/// thread pool. Created through the `ThreadPool::spawn_handle` function this handle will resolve
/// when the future provided resolves on the thread pool. If the remote thread restarts due to
/// panics, `SpawnError::Canceled` is returned.
#[derive(Debug)]
pub struct SpawnHandle<T, E>(oneshot::Receiver<Result<T, E>>);

impl<T, E> Future for SpawnHandle<T, E> {
    type Item = T;
    type Error = SpawnError<E>;

    fn poll(&mut self) -> Poll<T, SpawnError<E>> {
        match self.0.poll() {
            Ok(Async::Ready(Ok(item))) => Ok(Async::Ready(item)),
            Ok(Async::Ready(Err(error))) => Err(SpawnError::Error(error)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(_) => Err(SpawnError::Canceled),
        }
    }
}

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
    pub fn spawn_handle<F>(&self, future: F) -> SpawnHandle<F::Item, F::Error>
    where
        F: Future + Send + 'static,
        F::Item: Send + 'static,
        F::Error: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let future = future.then(|result| sender.send(result)).map_err(|_| ());

        match self.inner {
            Some(ref runtime) => runtime.executor().spawn(future),
            None => actix::spawn(future),
        }

        SpawnHandle(receiver)
    }
}
