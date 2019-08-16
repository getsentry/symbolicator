use std::time::{Duration, Instant};

use futures::{Async, Future, Poll};
use sentry::Hub;
use tokio::timer::timeout;

use crate::utils::sentry::SentryFutureExt;
use crate::utils::threadpool::{SpawnHandle, ThreadPool};

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
    inner: timeout::Timeout<F>,
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
            inner: timeout::Timeout::new(self, duration),
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
