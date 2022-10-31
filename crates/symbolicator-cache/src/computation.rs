use std::future::Future;
use std::hash::Hash;

use moka::future::Cache;

use crate::NeedsRefresh;

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
    type Output: NeedsRefresh + Clone + Send + Sync + 'static;

    /// The computation Future type.
    type Computation: Future<Output = Self::Output>;

    /// Returns the cache key corresponding to the `arg`.
    fn cache_key(&self, arg: &Self::Arg) -> Self::Key;

    /// Compute a new value that should be cached.
    fn compute(&self, arg: &Self::Arg) -> Self::Computation;
}

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
pub struct ComputationCache<D: ComputationDriver> {
    driver: D,
    computations: Cache<D::Key, D::Output>,
}

impl<D> ComputationCache<D>
where
    D: ComputationDriver,
{
    /// Creates a new Computation Cache.
    pub fn new(driver: D) -> Self {
        // TODO: make this configurable
        let computations = Cache::new(1_000);
        Self {
            driver,
            computations,
        }
    }

    /// Get or compute the output value for the provided `arg`.
    ///
    /// See [`ComputationCache`] docs for how the output value is computed and the panics that can happen.
    pub async fn get(&self, arg: &D::Arg) -> D::Output {
        let key = self.driver.cache_key(arg);

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
    impl<T> NeedsRefresh for RefreshesAfter<T> {
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
        type Computation = Pin<Box<dyn Future<Output = Self::Output>>>;

        fn cache_key(&self, _arg: &Self::Arg) -> Self::Key {}

        fn compute(&self, _arg: &Self::Arg) -> Self::Computation {
            let inner = self.calls.fetch_add(1, Ordering::Relaxed);

            Box::pin(async move {
                let ttl = Instant::now() + Duration::from_millis(10);

                RefreshesAfter { inner, ttl }
            })
        }
    }

    #[tokio::test]
    async fn test_refresh() {
        let driver = Driver {
            calls: Default::default(),
        };

        let cache = ComputationCache::new(driver);

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
