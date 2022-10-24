use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Mutex;

use futures::{future, FutureExt};
use tokio::runtime;
use tokio::task::JoinHandle;

use crate::time::Instant;
use crate::RefreshAfter;

/// The Cache Computation Driver
///
/// The driver is responsible for providing the actual computation that is supposed to be cached,
/// as well as determining the cache key.
pub trait ComputationDriver {
    /// Input argument to the driver.
    type Arg;
    /// Cache Key for the computation.
    type Key: Eq + Hash + Clone;
    /// The resulting output of the computation.
    type Output: RefreshAfter + Clone + Send + 'static;

    /// Returns the cache key corresponding to the `arg`.
    fn cache_key(&self, arg: &Self::Arg) -> Self::Key;

    /// Spawns the computation on the provided `runtime`.
    ///
    /// Right now the driver is responsible for spawning the computation just to prevent
    /// excessive boxing.
    /// In the future, once TAIT or RPITIT is stable, this will return an associated
    /// future, or be a straight up async fn.
    fn spawn_computation(
        &self,
        runtime: &runtime::Handle,
        arg: &Self::Arg,
    ) -> JoinHandle<Self::Output>;
}

// TODO: this should be `impl Future` once TAIT is stable.
//type ComputationFuture<V> = impl Future<Output = V>;
type ComputationFuture<V> = Pin<Box<dyn Future<Output = V>>>;
type CacheEntryComputation<V> = future::Shared<ComputationFuture<V>>;
// TODO: this needs to be some kind of lru that is bounded and evicted with time
type ComputationMap<K, V> = Mutex<HashMap<K, CacheEntryComputation<V>>>;

/// An in-memory Cache for async Computations.
///
/// The purpose of this Cache is to do request coalescing, and to hold the results
/// of the computation in-memory for some time depending on [`RefreshAfter`].
///
/// The cache is being constructed using a factory function that computes cache entries,
/// and a tokio runtime that will spawn those computations.
///
/// # Panics
///
/// The Cache will propagate panics and unwrap `JoinError`s from the provided
/// `factory` or `runtime`. It will also panic when its internal locking primitive has
/// been poisoned.
///
/// # TODO:
/// * provide a configurable bound for entries that are being kept in-memory after computation is complete
/// * evict cache entries based on usage and time
/// * be a bit smarter about `Clone`-ing the key
pub struct ComputationCache<D: ComputationDriver> {
    driver: D,
    runtime: runtime::Handle,
    computations: ComputationMap<D::Key, D::Output>,
}

impl<D> ComputationCache<D>
where
    D: ComputationDriver,
{
    /// Creates a new Computation Cache.
    pub fn new(runtime: tokio::runtime::Handle, driver: D) -> Self {
        Self {
            driver,
            runtime,
            computations: Default::default(),
        }
    }

    /// Get or compute the output value for the provided `arg`.
    ///
    /// See [`ComputationCache`] docs for how the output value is computed and the panics that can happen.
    pub async fn get(&self, arg: &D::Arg) -> D::Output {
        // This is a false positive:
        // We drop the lock right before the await.
        // We then re-lock if we need to refresh and loop around.
        // The lock is thus not held across an await point.
        // See https://github.com/rust-lang/rust-clippy/issues/9683
        #![allow(clippy::await_holding_lock)]

        let key = self.driver.cache_key(arg);

        let mut computations = self.computations.lock().unwrap();
        loop {
            let computation = {
                // TODO: avoid all these key clones and take key as `&K` ideally?
                // though that means we can’t use the entry API.
                let entry = computations.entry(key.clone());

                let computation = entry.or_insert_with(move || {
                    let fut = self.driver.spawn_computation(&self.runtime, arg);
                    // TODO: remove the Box::pin once TAIT is stable.
                    // XXX: for some reason we need an explicit type annotation here,
                    // so the compiler knows its supposed to be a `dyn Future`.
                    let fut: Pin<Box<dyn Future<Output = D::Output>>> = Box::pin(async move {
                        // unwrap the `JoinError` as we want the fn signature to be as
                        // clean as possible.
                        fut.await.unwrap()
                    });
                    fut.shared()
                });
                computation.clone()
            };
            drop(computations);

            let value = computation.await;

            match value.refresh_after() {
                Some(refresh) if refresh < Instant::now() => {
                    computations = self.computations.lock().unwrap();
                    computations.remove(&key);
                    // we loop around and create a new computation
                    continue;
                }
                _ => return value,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::time::{self, Duration};

    use super::*;

    #[derive(Clone)]
    struct RefreshesAfter<T> {
        pub inner: T,
        refresh_after: Option<Instant>,
    }
    impl<T> RefreshAfter for RefreshesAfter<T> {
        fn refresh_after(&self) -> Option<Instant> {
            self.refresh_after
        }
    }
    struct Driver {
        calls: AtomicUsize,
    }
    impl ComputationDriver for Driver {
        type Arg = ();
        type Key = ();
        type Output = RefreshesAfter<usize>;

        fn cache_key(&self, _arg: &Self::Arg) -> Self::Key {}

        fn spawn_computation(
            &self,
            runtime: &runtime::Handle,
            _arg: &Self::Arg,
        ) -> JoinHandle<Self::Output> {
            let inner = self.calls.fetch_add(1, Ordering::Relaxed);

            runtime.spawn(async move {
                let refresh_after = Some(Instant::now() + Duration::from_millis(10));

                RefreshesAfter {
                    inner,
                    refresh_after,
                }
            })
        }
    }

    #[tokio::test]
    async fn test_refresh() {
        let driver = Driver {
            calls: Default::default(),
        };

        let cache = ComputationCache::new(tokio::runtime::Handle::current(), driver);

        time::pause();
        let res = futures::join!(cache.get(&()), cache.get(&()), cache.get(&()));
        assert_eq!((res.0.inner, res.1.inner, res.2.inner), (0, 0, 0));

        time::advance(Duration::from_millis(5)).await;
        assert_eq!(cache.get(&()).await.inner, 0);

        time::advance(Duration::from_millis(10)).await;

        let res = futures::join!(cache.get(&()), cache.get(&()));
        assert_eq!((res.0.inner, res.1.inner), (1, 1));
    }
}
