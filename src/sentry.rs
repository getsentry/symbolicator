use std::sync::Arc;

use futures::future::Future;
use futures::Poll;

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

pub trait WriteSentryScope {
    fn write_sentry_scope(&self, scope: &mut Scope);
}
