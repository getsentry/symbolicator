use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Instant;

use futures::{future, FutureExt};

/// This trait signals the [`ComputationCache`] when to refresh its entries.
pub trait RefreshAfter {
    /// Tells the [`ComputationCache`] if and when to refresh its entry.
    fn refresh_after(&self) -> Option<Instant> {
        None
    }
}

type ComputationFactory<K, V> = Box<dyn Fn(K) -> CacheEntryComputation<V>>;
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
pub struct ComputationCache<K, V> {
    computation_factory: ComputationFactory<K, V>,
    computations: ComputationMap<K, V>,
}

impl<K, V> ComputationCache<K, V>
where
    K: Eq + Hash + Clone,
    V: RefreshAfter + Clone + Send + 'static,
{
    /// Creates a new Cache.
    ///
    /// If a requested item does not yet exists in the cache, or needs to be refreshed,
    /// the `factory` function is being invoked and the resulting future is spawned on
    /// the provided `runtime`.
    pub fn new<F, Fut>(runtime: tokio::runtime::Handle, factory: F) -> Self
    where
        F: Fn(K) -> Fut + 'static,
        Fut: Future<Output = V> + Send + 'static,
    {
        let computation_factory = Box::new(move |key| {
            let fut = runtime.spawn(factory(key));
            // unwrap the `JoinError`
            let fut = fut.map(Result::unwrap);
            // TODO: remove this once TAIT is stable.
            let fut: Pin<Box<dyn Future<Output = V>>> = Box::pin(fut);
            fut.shared()
        });
        Self {
            computation_factory,
            computations: Default::default(),
        }
    }

    /// Get or compute the value for the provided `key`.
    ///
    /// See [`ComputationCache`] docs for how the value is computed and the panics that can happen.
    pub async fn get(&self, key: K) -> V {
        // This is a false positive:
        // We drop the lock right before the await.
        // We then re-lock if we need to refresh and loop around.
        // The lock is thus not held across an await point.
        // See https://github.com/rust-lang/rust-clippy/issues/9683
        #![allow(clippy::await_holding_lock)]

        let mut computations = self.computations.lock().unwrap();
        loop {
            let computation = {
                // TODO: avoid all these key clones and take key as `&K` ideally
                let entry = computations.entry(key.clone());
                let computation_key = key.clone();

                let computation =
                    entry.or_insert_with(move || (self.computation_factory)(computation_key));
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
    use std::time::Duration;

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

    #[tokio::test]
    async fn test_refresh() {
        let calls = AtomicUsize::default();
        let cache = ComputationCache::new(tokio::runtime::Handle::current(), move |_key: ()| {
            let refresh_after = Some(Instant::now() + Duration::from_millis(20));
            let inner = calls.fetch_add(1, Ordering::Relaxed);
            async move {
                RefreshesAfter {
                    inner,
                    refresh_after,
                }
            }
        });

        let res = futures::join!(cache.get(()), cache.get(()), cache.get(()));
        assert_eq!((res.0.inner, res.1.inner, res.2.inner), (0, 0, 0));

        // XXX: this might be a bit flaky due to timings
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(cache.get(()).await.inner, 0);

        tokio::time::sleep(Duration::from_millis(20)).await;

        let res = futures::join!(cache.get(()), cache.get(()));
        assert_eq!((res.0.inner, res.1.inner), (1, 1));
    }
}
