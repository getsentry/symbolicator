use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::{Future, Poll};
use tokio_threadpool::{SpawnHandle as TokioHandle, ThreadPool as TokioPool};

static IS_TEST: AtomicBool = AtomicBool::new(false);

/// Enables test mode of all thread pools.
///
/// In this mode, threadpools do not spawn any futures into threads, but instead just return them.
/// This is useful to ensure deterministic test execution, and also allows to capture console output
/// from spawned tasks.
#[cfg(test)]
pub fn enable_test_mode() {
    IS_TEST.store(true, Ordering::Relaxed);
}

/// Work-stealing based thread pool for executing futures.
#[derive(Clone, Debug)]
pub struct ThreadPool {
    inner: Option<Arc<TokioPool>>,
}

impl ThreadPool {
    /// Create a new `ThreadPool` with default values.
    pub fn new() -> Self {
        let inner = if IS_TEST.load(Ordering::Relaxed) {
            None
        } else {
            Some(Arc::new(TokioPool::new()))
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
        SpawnHandle(match self.inner {
            Some(ref pool) => SpawnHandleInner::Tokio(pool.spawn_handle(future)),
            None => SpawnHandleInner::Future(Box::new(future)),
        })
    }
}

enum SpawnHandleInner<T, E> {
    Tokio(TokioHandle<T, E>),
    Future(Box<dyn Future<Item = T, Error = E>>),
}

impl<T, E> fmt::Debug for SpawnHandleInner<T, E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SpawnHandleInner::Tokio(_) => write!(f, "SpawnHandle::Tokio(tokio::SpawnHandle)"),
            SpawnHandleInner::Future(_) => write!(f, "SpawnHandle::Future(dyn Future)"),
        }
    }
}

/// Handle returned from `ThreadPool::spawn_handle`.
///
/// This handle is a future representing the completion of a different future spawned on to the
/// thread pool. Created through the `ThreadPool::spawn_handle` function this handle will resolve
/// when the future provided resolves on the thread pool.
pub struct SpawnHandle<T, E>(SpawnHandleInner<T, E>);

impl<T, E> fmt::Debug for SpawnHandle<T, E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T, E> Future for SpawnHandle<T, E> {
    type Item = T;
    type Error = E;

    fn poll(&mut self) -> Poll<T, E> {
        match self.0 {
            SpawnHandleInner::Tokio(ref mut f) => f.poll(),
            SpawnHandleInner::Future(ref mut f) => f.poll(),
        }
    }
}
