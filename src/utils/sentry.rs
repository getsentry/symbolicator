// use std::ops::Deref;
use std::sync::Arc;

// use actix_web::{error, web, Error, FromRequest, HttpRequest};
use futures::{Future, Poll};
use sentry::{Hub, Scope};

/// A future that bind a `Hub` to its execution.
#[derive(Debug)]
pub struct SentryFuture<F> {
    hub: Arc<Hub>,
    future: F,
}

impl<F> SentryFuture<F> {
    /// Creates a new bound future with a `Hub`.
    pub fn new(hub: Arc<Hub>, future: F) -> Self {
        Self { hub, future }
    }
}

impl<F> Future for SentryFuture<F>
where
    F: Future,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        Hub::run(self.hub.clone(), || self.future.poll())
    }
}

/// Future extensions for Sentry.
pub trait SentryFutureExt: Sized {
    /// Binds a hub to the execution of this future.
    ///
    /// This ensures that the future is polled within the given hub.
    fn bind_hub<H>(self, hub: H) -> SentryFuture<Self>
    where
        H: Into<Arc<Hub>>,
    {
        SentryFuture {
            future: self,
            hub: hub.into(),
        }
    }
}

impl<F> SentryFutureExt for F where F: Future {}

/// Configures the sentry `Scope` with data from this object.
pub trait ToSentryScope {
    /// Writes data to the given scope.
    ///
    /// This can be called inside `sentry::configure_scope`. There is also a shorthand
    /// `.configure_scope` in this trait.
    fn to_scope(&self, scope: &mut Scope);

    /// Configures the current scope.
    fn configure_scope(&self) {
        sentry::configure_scope(|scope| self.to_scope(scope));
    }

    /// Configures the top scope on the given hub.
    fn configure_hub(&self, hub: &Hub) {
        hub.configure_scope(|scope| self.to_scope(scope));
    }
}
