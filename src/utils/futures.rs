use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cadence::{prelude::*, Metric, MetricBuilder};
use futures::{Async, Future, Poll};
use tokio::timer::Timeout as TokioTimeout;
use tokio_threadpool::{SpawnHandle as TokioHandle, ThreadPool as TokioPool};

use crate::metrics;

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
///
/// The callback must not panic under any circumstance. Since it is called while dropping an item,
/// this might result in aborting program execution.
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

/// The completion result of a future.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum FutureCompletion {
    /// The future resolved to an item.
    Ok,
    /// The future resolved to an error.
    Error,
    /// The future timed out during execution.
    Timeout,
    /// The future was dropped before completing.
    Dropped,
}

impl FutureCompletion {
    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "err",
            Self::Timeout => "timeout",
            Self::Dropped => "dropped",
        }
    }
}

/// The state of a measured future.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum FutureState {
    /// The future is waiting to be polled for the first time.
    Waiting(Instant),
    /// The future has been polled and is computing.
    Started(Instant),
    /// The future has terminated.
    Done(FutureCompletion),
}

impl Default for FutureState {
    fn default() -> Self {
        FutureState::Waiting(Instant::now())
    }
}

/// A builder-like map for metrics tags.
#[derive(Clone, Debug, Default)]
pub struct TagMap(BTreeMap<&'static str, Cow<'static, str>>);

impl TagMap {
    /// Creates a new tag map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a new tag to the map, returning the instance.
    pub fn add<S>(mut self, tag: &'static str, value: S) -> Self
    where
        S: Into<Cow<'static, str>>,
    {
        self.0.insert(tag, value.into());
        self
    }
}

/// Extension trait to add multiple tags to a metrics builder.
trait MetricBuilderExt<'m> {
    /// Adds all given tags to the metrics builder.
    fn with_tags(self, tags: &'m TagMap) -> Self;
}

impl<'m, 'c, T> MetricBuilderExt<'m> for MetricBuilder<'m, 'c, T>
where
    T: Metric + From<String>,
{
    fn with_tags(mut self, tags: &'m TagMap) -> Self {
        for (tag, value) in &tags.0 {
            self = self.with_tag(tag, &value);
        }
        self
    }
}

/// State machine for measuring futures.
#[derive(Debug, Clone)]
struct FutureMetric {
    task_name: &'static str,
    state: FutureState,
    tags: TagMap,
}

impl FutureMetric {
    /// Creates a new future metric without tags.
    #[allow(unused)]
    pub fn new(task_name: &'static str) -> Self {
        Self::tagged(task_name, TagMap::new())
    }

    /// Creates a new future metric with custom tags.
    pub fn tagged(task_name: &'static str, tags: TagMap) -> Self {
        FutureMetric {
            task_name,
            state: FutureState::Waiting(Instant::now()),
            tags,
        }
    }

    /// Indicates the start of future execution.
    ///
    /// If called multiple times, only the first call is recorded. A `futures.wait_time` metric is
    /// emitted.
    pub fn start(&mut self) {
        if let FutureState::Waiting(creation_time) = self.state {
            let elapsed = creation_time.elapsed();
            self.state = FutureState::Started(Instant::now());

            metrics::with_client(|client| {
                client
                    .time_duration_with_tags("futures.wait_time", elapsed)
                    .with_tags(&self.tags)
                    .with_tag("task_name", self.task_name)
                    .send();
            });
        }
    }

    /// Indicates that the future has terminated with the given completion.
    ///
    /// If called multiple times, only the first call is recorded. A `futures.done` metric is
    /// emitted for the configured task.
    pub fn complete(&mut self, completion: FutureCompletion) {
        let elapsed = match self.state {
            FutureState::Waiting(creation_time) => creation_time.elapsed(),
            FutureState::Started(start_time) => start_time.elapsed(),
            FutureState::Done(_) => return,
        };

        metrics::with_client(|client| {
            client
                .time_duration_with_tags("futures.done", elapsed)
                .with_tags(&self.tags)
                .with_tag("task_name", self.task_name)
                .with_tag("status", completion.name())
                .send();
        });

        self.state = FutureState::Done(completion);
    }
}

/// Emits a `futures.done` metric if dropped before completion.
impl Drop for FutureMetric {
    fn drop(&mut self) {
        self.complete(FutureCompletion::Dropped);
    }
}

/// A measured `Future` that emits metrics when it starts and completes.
pub struct Measured<F> {
    inner: F,
    metric: FutureMetric,
}

impl<F> Measured<F>
where
    F: Future,
{
    /// Creates a `Future` that execute for a limited time.
    ///
    /// If the future completes before the timeout has expired, then `Timeout` returns the completed
    /// value. Otherwise, it returns the error by invoking the callback. The timeout is measured as
    /// a separate completion state in the metric.
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
                self.metric.complete(FutureCompletion::Ok);
                Ok(Async::Ready(item))
            }
            Err(error) => {
                self.metric.complete(FutureCompletion::Error);
                Err(error)
            }
        }
    }
}

/// A `Future` that only executes for a limited time.
pub struct Timeout<F, E> {
    inner: TokioTimeout<F>,
    map_err: Option<E>,
    metric: Option<FutureMetric>,
}

impl<F, E> Timeout<F, E> {
    /// Creates a `Future` that measures its execution timing.
    ///
    /// There are two metrics that are being recorded:
    ///
    ///  - `futures.wait_time` when the future polls for the first time. This indicates how long a
    ///    future had to wait for execution, e.g. in the queue of a thread pool.
    ///  - `futures.done` when the future completes or terminates. This also logs the completion
    ///    state as a tag: `ok`, `err`, `timeout` or `dropped`.
    pub fn measure(self, task_name: &'static str) -> Self {
        self.measure_tagged(task_name, TagMap::new())
    }

    /// Creates a `Future` that measures its execution timing.
    ///
    /// This is the same as `measure`, except with a custom list of tags.
    pub fn measure_tagged(mut self, task_name: &'static str, tags: TagMap) -> Self {
        self.metric = Some(FutureMetric::tagged(task_name, tags));
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
                    metric.complete(FutureCompletion::Ok);
                }
                Ok(Async::Ready(item))
            }
            Err(error) => match error.into_inner() {
                Some(error) => {
                    if let Some(ref mut metric) = self.metric {
                        metric.complete(FutureCompletion::Error);
                    }
                    Err(error)
                }
                None => {
                    if let Some(ref mut metric) = self.metric {
                        metric.complete(FutureCompletion::Timeout);
                    }
                    Err(self.map_err.take().unwrap()().into())
                }
            },
        }
    }
}

/// Extensions on the `Future` trait.
pub trait FutureExt: Future {
    /// Creates a `Future` that measures its execution timing.
    ///
    /// There are two metrics that are being recorded:
    ///
    ///  - `futures.wait_time` when the future polls for the first time. This indicates how long a
    ///    future had to wait for execution, e.g. in the queue of a thread pool.
    ///  - `futures.done` when the future completes or terminates. This also logs the completion
    ///    state as a tag: `ok`, `err`, `timeout` or `dropped`.
    #[inline]
    fn measure(self, task_name: &'static str) -> Measured<Self>
    where
        Self: Sized,
    {
        self.measure_tagged(task_name, TagMap::new())
    }

    /// Creates a `Future` that measures its execution timing.
    ///
    /// This is the same as `measure`, except with a custom list of tags.
    fn measure_tagged(self, task_name: &'static str, tags: TagMap) -> Measured<Self>
    where
        Self: Sized,
    {
        Measured {
            inner: self,
            metric: FutureMetric::tagged(task_name, tags),
        }
    }

    /// Creates a `Future` that execute for a limited time.
    ///
    /// If the future completes before the timeout has expired, then `Timeout` returns the completed
    /// value. Otherwise, it returns the error by invoking the callback.
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

    /// Spawns the future on the given thread pool.
    ///
    /// The currently active `sentry::Hub` is propagated to the spawned future (shared).
    fn spawn_on(self, pool: &ThreadPool) -> SpawnHandle<Self::Item, Self::Error>
    where
        Self: Sized + Send + 'static,
        Self::Item: Send + 'static,
        Self::Error: Send + 'static,
    {
        pool.spawn_handle(self)
    }
}

impl<F> FutureExt for F where F: Future {}
