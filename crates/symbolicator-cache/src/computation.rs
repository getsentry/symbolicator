use std::hash::Hash;
use std::{future::Future, sync::Arc};

use moka::future::{Cache, CacheBuilder};

/// This trait signals the [`ComputationCache`] when and which of its entries to
/// evict and refresh.
pub trait CacheEntry {
    /// Tells the [`ComputationCache`] if an entry should be refreshed.
    fn needs_refresh(&self) -> bool {
        false
    }

    /// Gives a relative weight for the cache entry, as not all entries are created equal.
    fn weight(&self) -> u32 {
        1
    }
}

/// The Cache Computation Driver
///
/// The driver is responsible for providing the actual computation that is supposed to be cached,
/// as well as determining the cache key.
pub trait ComputationDriver {
    /// Input argument to the driver.
    type Arg;
    /// Cache Key for the computation.
    type Key: Eq + Hash + Send + Sync + 'static;
    /// The resulting output of the computation.
    type Output: CacheEntry + Send + Sync + 'static;
    /// The computation Future type.
    type Computation: Future<Output = Arc<Self::Output>>;

    /// Returns the cache key corresponding to the `arg`.
    fn cache_key(&self, arg: &Self::Arg) -> Self::Key;

    /// Compute a new value that should be cached.
    fn compute(&self, arg: Self::Arg) -> Self::Computation;
}

/// An in-memory Cache for async Computations.
///
/// The purpose of this Cache is to do request coalescing, and to hold the results
/// of the computation in-memory for some time depending on [`NeedsRefresh`].
///
/// The cache is being constructed using a [`ComputationDriver`] that provides cache keys
/// and a way to compute new cache values on demand.
#[derive(Debug)]
pub struct ComputationCache<D: ComputationDriver> {
    driver: D,
    computations: Cache<D::Key, Arc<D::Output>>,
}

impl<D> ComputationCache<D>
where
    D: ComputationDriver,
{
    /// Creates a new Computation Cache.
    pub fn new(driver: D, capacity: u64) -> Self {
        let computations = CacheBuilder::new(capacity)
            .weigher(|_k, v: &Arc<D::Output>| v.weight())
            .build();
        Self {
            driver,
            computations,
        }
    }

    /// Get or compute the output value for the provided `arg`.
    ///
    /// See [`ComputationCache`] docs for how the output value is computed and the panics that can happen.
    pub async fn get(&self, arg: D::Arg) -> Arc<D::Output> {
        let key = self.driver.cache_key(&arg);

        self.computations
            .get_with_if(key, async { self.driver.compute(arg).await }, |v| {
                v.needs_refresh()
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::time::{self, Duration, Instant};

    use super::*;

    #[derive(Clone)]
    struct RefreshesAfter<T> {
        pub inner: T,
        ttl: Instant,
    }
    impl<T> CacheEntry for RefreshesAfter<T> {
        fn needs_refresh(&self) -> bool {
            Instant::now() >= self.ttl
        }
    }
    struct Driver {
        calls: AtomicUsize,
    }
    impl ComputationDriver for Driver {
        type Arg = ();
        type Key = ();
        type Output = RefreshesAfter<usize>;
        type Computation = Pin<Box<dyn Future<Output = Arc<Self::Output>>>>;

        fn cache_key(&self, _arg: &Self::Arg) -> Self::Key {}

        fn compute(&self, _arg: Self::Arg) -> Self::Computation {
            let inner = self.calls.fetch_add(1, Ordering::Relaxed);

            Box::pin(async move {
                let ttl = Instant::now() + Duration::from_millis(10);

                Arc::new(RefreshesAfter { inner, ttl })
            })
        }
    }

    #[tokio::test]
    async fn test_refresh() {
        let driver = Driver {
            calls: Default::default(),
        };

        let cache = ComputationCache::new(driver, 1_000);

        time::pause();
        let res = futures::join!(cache.get(()), cache.get(()), cache.get(()));
        assert_eq!((res.0.inner, res.1.inner, res.2.inner), (0, 0, 0));

        time::advance(Duration::from_millis(5)).await;
        assert_eq!(cache.get(()).await.inner, 0);

        time::advance(Duration::from_millis(10)).await;

        let res = futures::join!(cache.get(()), cache.get(()));
        assert_eq!((res.0.inner, res.1.inner), (1, 1));
    }
}
