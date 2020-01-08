use std::future::Future as StdFuture;
use std::sync::Arc;

use futures01::future::Future;
use futures01::Poll;

use sentry::{Hub, Scope};

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

pub trait SentryFutureExt: Sized {
    fn sentry_hub(self, hub: Arc<Hub>) -> SentryFuture<Self> {
        SentryFuture { inner: self, hub }
    }

    fn sentry_hub_current(self) -> SentryFuture<Self> {
        self.sentry_hub(Hub::current())
    }

    fn sentry_hub_new_from_current(self) -> SentryFuture<Self> {
        self.sentry_hub(Arc::new(Hub::new_from_top(Hub::current())))
    }
}

impl<F> SentryFutureExt for F {}

/// Write own data to Sentry scope, only the subset that is considered useful for debugging. Right
/// now this could've been a simple method, but the idea is that one day we want a custom derive
/// for this.
pub trait WriteSentryScope {
    fn write_sentry_scope(&self, scope: &mut Scope);
}

pub async fn with_hub<F, H>(hub: H, future: F) -> F::Output
where
    F: StdFuture,
    H: Into<Arc<Hub>>,
{
    let hub = hub.into();
    futures::pin_mut!(future);

    let poll = futures::future::poll_fn(|ctx| {
        let future = &mut future;
        Hub::run(hub.clone(), || future.as_mut().poll(ctx))
    });

    poll.await
}
