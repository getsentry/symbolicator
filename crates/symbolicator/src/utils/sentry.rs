#![allow(deprecated)]

use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use sentry::integrations::backtrace::parse_stacktrace;
use sentry::protocol::{ClientSdkPackage, Event, Exception};
use sentry::{parse_type_from_debug, Hub, Level, Scope, ScopeGuard};
use uuid::Uuid;

pub struct SentryFuture<F> {
    pub(crate) hub: Arc<Hub>,
    pub(crate) inner: F,
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
        self.bind_hub(Hub::new_from_top(Hub::current()))
    }
}

/// Write own data to [`sentry::Scope`], only the subset that is considered useful for debugging.
pub trait ConfigureScope {
    /// Writes information to the given scope.
    fn to_scope(&self, scope: &mut Scope);

    /// Configures the current scope.
    fn configure_scope(&self) {
        sentry::configure_scope(|scope| self.to_scope(scope));
    }
}
