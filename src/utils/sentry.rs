use std::sync::Arc;

use futures01::future::Future;
use futures01::Poll;

use sentry::{Hub, Scope};

// TODO: where the fuck is the warning about the missing Debug impl???
pub struct SentryFuture<F> {
    pub(crate) hub: Arc<Hub>,
    pub(crate) inner: F,
}

impl<F> Future for SentryFuture<F>
where
    F: Future,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        Hub::run(self.hub.clone(), || self.inner.poll())
    }
}

impl<F> std::future::Future for SentryFuture<F>
where
    F: futures::future::Future,
{
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        ctx: &mut std::task::Context,
    ) -> std::task::Poll<Self::Output> {
        let hub = self.hub.clone();
        // https://doc.rust-lang.org/std/pin/index.html#pinning-is-structural-for-field
        let future = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        Hub::run(hub, || future.poll(ctx))
    }
}

pub trait SentryFutureExt: Sized {
    fn bind_hub<H>(self, hub: H) -> SentryFuture<Self>
    where
        H: Into<Arc<Hub>>,
    {
        SentryFuture {
            inner: self,
            hub: hub.into(),
        }
    }

    fn sentry_hub_current(self) -> SentryFuture<Self> {
        self.bind_hub(Hub::current())
    }

    fn sentry_hub_new_from_current(self) -> SentryFuture<Self> {
        self.bind_hub(Arc::new(Hub::new_from_top(Hub::current())))
    }
}

impl<F> SentryFutureExt for F where F: futures01::future::Future {}

// The same as `SentryFutureExt` but we can not re-use the trait because the blanket
// implementations would conflict.
pub trait SentryStdFutureExt: Sized {
    fn bind_hub<H>(self, hub: H) -> SentryFuture<Self>
    where
        H: Into<Arc<Hub>>,
    {
        SentryFuture {
            inner: self,
            hub: hub.into(),
        }
    }

    fn sentry_hub_current(self) -> SentryFuture<Self> {
        self.bind_hub(Hub::current())
    }

    fn sentry_hub_new_from_current(self) -> SentryFuture<Self> {
        self.bind_hub(Arc::new(Hub::new_from_top(Hub::current())))
    }
}

impl<F> SentryStdFutureExt for F where F: std::future::Future {}

/// Write own data to Sentry scope, only the subset that is considered useful for debugging. Right
/// now this could've been a simple method, but the idea is that one day we want a custom derive
/// for this.
pub trait WriteSentryScope {
    fn write_sentry_scope(&self, scope: &mut Scope);
}
