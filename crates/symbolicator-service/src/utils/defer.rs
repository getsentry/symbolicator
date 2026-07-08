use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Guard that runs a closure when dropped.
///
/// The closure must not panic under any circumstance. Since it is called while dropping an item,
/// this might result in aborting program execution.
pub struct DeferGuard<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for DeferGuard<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f()
        }
    }
}

/// Defers a closure, returning a `DeferGuard` that will
/// run it when dropped.
pub fn defer<F: FnOnce()>(f: F) -> DeferGuard<F> {
    DeferGuard(Some(f))
}

/// A shared counter similar to a semaphore.
///
/// The counter can be temporarily incremented and is automatically decremented again.
#[derive(Debug, Clone)]
pub struct DeferCounter(Arc<AtomicUsize>);

impl DeferCounter {
    /// Creates a new [`DeferCounter`] with a value of `0`.
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    /// Returns the current value of the counter.
    pub fn value(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Increments the counter and returns a [`DeferCounterToken`].
    ///
    /// When the returned token is dropped the counter will be decremented.
    pub fn incr(&self) -> DeferCounterToken {
        let counter = Some(self.clone());
        let _ = self.0.fetch_add(1, Ordering::Relaxed);
        DeferCounterToken { counter }
    }

    /// Attempts to increment the counter without exceeding `max`.
    ///
    /// Returns `None` if the counter is already at or above `max`.
    /// A `None` max, will always increment the counter.
    ///
    /// When the returned token is dropped the counter will be decremented.
    pub fn try_incr(&self, max: Option<usize>) -> Option<DeferCounterToken> {
        let Some(max) = max else {
            return Some(self.incr());
        };

        self.0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                (value < max).then_some(value + 1)
            })
            .ok()?;

        let counter = Some(self.clone());
        Some(DeferCounterToken { counter })
    }
}

impl Default for DeferCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// A token which automatically decrements a [`DeferCounter`] on drop.
pub struct DeferCounterToken {
    counter: Option<DeferCounter>,
}

impl Drop for DeferCounterToken {
    fn drop(&mut self) {
        if let Some(counter) = self.counter.take() {
            // Sanity check we should never be here and already be at a `0` count; each increment
            // must only be followed by a single decrement.
            debug_assert_ne!(counter.0.load(std::sync::atomic::Ordering::Relaxed), 0);
            counter.0.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// A [`DeferCounter`] which cannot be incremented past a specified maximum.
#[derive(Debug, Clone)]
pub struct MaxDeferCounter {
    max: Option<usize>,
    counter: DeferCounter,
}

impl MaxDeferCounter {
    /// Creates a new [`MaxDeferCounter`] with the specified `max`.
    ///
    /// A `None` maximum will always allow incrementing the counter.
    pub fn new(max: Option<usize>) -> Self {
        Self {
            max,
            counter: DeferCounter::new(),
        }
    }

    /// Returns the current value of the counter.
    pub fn value(&self) -> usize {
        self.counter.value()
    }

    /// Attempts to increment the counter without exceeding the configured `max`.
    ///
    /// Returns `None` if the counter is already at or above `max`.
    ///
    /// When the returned token is dropped the counter will be decremented.
    pub fn try_incr(&self) -> Option<DeferCounterToken> {
        self.counter.try_incr(self.max)
    }
}

impl Default for MaxDeferCounter {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{DeferCounter, MaxDeferCounter};

    #[test]
    fn test_defer_counter() {
        let counter = DeferCounter::new();
        assert_eq!(counter.value(), 0);

        let token = counter.incr();
        assert_eq!(counter.value(), 1);

        drop(token);
        assert_eq!(counter.value(), 0);
    }

    #[test]
    fn test_defer_counter_try_incr() {
        let counter = DeferCounter::new();

        let token = counter.try_incr(Some(1)).unwrap();
        assert_eq!(counter.value(), 1);

        assert!(counter.try_incr(Some(1)).is_none());

        drop(token);
        assert_eq!(counter.value(), 0);
        let _ = counter.try_incr(Some(1)).unwrap();
    }

    #[test]
    fn test_max_defer_counter() {
        let counter = MaxDeferCounter::new(Some(1));

        let token = counter.try_incr().unwrap();
        assert_eq!(counter.value(), 1);

        assert!(counter.try_incr().is_none());

        drop(token);
        assert_eq!(counter.value(), 0);
        let _ = counter.try_incr().unwrap();
    }

    #[test]
    fn test_max_defer_counter_none() {
        let counter = MaxDeferCounter::new(None);

        for _ in 0..u16::MAX {
            std::mem::forget(counter.try_incr().unwrap());
        }

        assert_eq!(counter.value(), u16::MAX as usize);
    }
}
