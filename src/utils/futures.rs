use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Async, Future, Poll};
use sentry::Hub;
use tokio::timer::Timeout as TokioTimeout;
use tokio_threadpool::{SpawnHandle as TokioHandle, ThreadPool as TokioPool};

use crate::utils::sentry::SentryFutureExt;

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
        let inner = if cfg!(test) && IS_TEST.load(Ordering::Relaxed) {
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

/// Execute a callback on dropping of the container type.
pub struct CallOnDrop {
    f: Option<Box<dyn FnOnce() + 'static>>,
}

impl CallOnDrop {
    pub fn new<F: FnOnce() + 'static>(f: F) -> CallOnDrop {
        CallOnDrop {
            f: Some(Box::new(f)),
        }
    }
}

impl Drop for CallOnDrop {
    fn drop(&mut self) {
        (self.f.take().unwrap())();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum FutureMetricResult {
    Ok,
    Error,
    Timeout,
    Dropped,
}

impl FutureMetricResult {
    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "err",
            Self::Timeout => "timeout",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum FutureMetricState {
    Waiting(Instant),
    Started(Instant),
    Done(FutureMetricResult),
}

impl Default for FutureMetricState {
    fn default() -> Self {
        FutureMetricState::Waiting(Instant::now())
    }
}

#[derive(Debug, Clone)]
struct FutureMetric<'a> {
    task_name: &'a str,
    state: FutureMetricState,
}

impl<'a> FutureMetric<'a> {
    pub fn new(task_name: &'a str) -> Self {
        FutureMetric {
            task_name,
            state: FutureMetricState::Waiting(Instant::now()),
        }
    }

    pub fn start(&mut self) {
        if let FutureMetricState::Waiting(creation_time) = self.state {
            let elapsed = creation_time.elapsed();
            self.state = FutureMetricState::Started(Instant::now());
            metric!(
                timer("futures.wait_time") = elapsed,
                "task_name" => self.task_name,
            );
        }
    }

    pub fn finish(&mut self, result: FutureMetricResult) {
        let elapsed = match self.state {
            FutureMetricState::Waiting(creation_time) => creation_time.elapsed(),
            FutureMetricState::Started(start_time) => start_time.elapsed(),
            FutureMetricState::Done(_) => return,
        };

        metric!(
            timer("futures.done") = elapsed,
            "task_name" => self.task_name,
            "status" => result.name(),
        );
        self.state = FutureMetricState::Done(result);
    }
}

impl<'a> Drop for FutureMetric<'a> {
    fn drop(&mut self) {
        self.finish(FutureMetricResult::Dropped);
    }
}

pub struct Measured<F> {
    inner: F,
    metric: FutureMetric<'static>,
}

impl<F> Measured<F>
where
    F: Future,
{
    #[allow(unused)]
    #[deprecated(note = "call .timeout() first, then .measure()")]
    pub fn timeout<E, R>(self, duration: Duration, map_err: E) -> Timeout<F, E>
    where
        Self: Sized,
        E: FnOnce() -> R,
        R: Into<F::Error>,
    {
        let mut timeout = self.inner.timeout(duration, map_err);
        timeout.metric = Some(self.metric);
        timeout
    }
}

impl<F> Future for Measured<F>
where
    F: Future,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.metric.start();
        match self.inner.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(item)) => {
                self.metric.finish(FutureMetricResult::Ok);
                Ok(Async::Ready(item))
            }
            Err(error) => {
                self.metric.finish(FutureMetricResult::Error);
                Err(error)
            }
        }
    }
}

pub struct Timeout<F, E> {
    inner: TokioTimeout<F>,
    map_err: Option<E>,
    metric: Option<FutureMetric<'static>>,
}

impl<F, E> Timeout<F, E> {
    pub fn measure(mut self, task_name: &'static str) -> Self {
        self.metric = Some(FutureMetric::new(task_name));
        self
    }
}

impl<F, E, R> Future for Timeout<F, E>
where
    F: Future,
    E: FnOnce() -> R,
    R: Into<F::Error>,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(ref mut metric) = self.metric {
            metric.start();
        }

        match self.inner.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(item)) => {
                if let Some(ref mut metric) = self.metric {
                    metric.finish(FutureMetricResult::Ok);
                }
                Ok(Async::Ready(item))
            }
            Err(error) => match error.into_inner() {
                Some(error) => {
                    if let Some(ref mut metric) = self.metric {
                        metric.finish(FutureMetricResult::Error);
                    }
                    Err(error)
                }
                None => {
                    if let Some(ref mut metric) = self.metric {
                        metric.finish(FutureMetricResult::Timeout);
                    }
                    Err(self.map_err.take().unwrap()().into())
                }
            },
        }
    }
}

pub trait FutureExt: Future {
    fn measure(self, task_name: &'static str) -> Measured<Self>
    where
        Self: Sized,
    {
        Measured {
            inner: self,
            metric: FutureMetric::new(task_name),
        }
    }

    fn timeout<E, R>(self, duration: Duration, map_err: E) -> Timeout<Self, E>
    where
        Self: Sized,
        E: FnOnce() -> R,
        R: Into<Self::Error>,
    {
        Timeout {
            inner: TokioTimeout::new(self, duration),
            map_err: Some(map_err),
            metric: None,
        }
    }

    fn spawn_on(self, pool: &ThreadPool) -> SpawnHandle<Self::Item, Self::Error>
    where
        Self: Sized + Send + 'static,
        Self::Item: Send + 'static,
        Self::Error: Send + 'static,
    {
        pool.spawn_handle(self.bind_hub(Hub::current()))
    }
}

impl<F> FutureExt for F where F: Future {}
