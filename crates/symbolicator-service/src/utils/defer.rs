/// Guard that runs a closure when dropped.
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
