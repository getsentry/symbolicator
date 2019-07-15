// use std::ops::Deref;
use std::sync::Arc;

// use actix_web::{error, web, Error, FromRequest, HttpRequest};
use futures::{Future, Poll};
use sentry::{Hub, Scope};

// #[derive(Clone, Debug)]
// pub struct RequestHub(pub Arc<Hub>);

// impl RequestHub {
//     pub fn hub(&self) -> Arc<Hub> {
//         self.0.clone()
//     }
// }

// impl Deref for RequestHub {
//     type Target = Hub;

//     fn deref(&self) -> &Hub {
//         &self.0
//     }
// }

// impl Into<Arc<Hub>> for RequestHub {
//     fn into(self) -> Arc<Hub> {
//         self.0
//     }
// }

// impl FromRequest for RequestHub {
//     type Config = ();
//     type Error = Error;
//     type Future = Result<Self, Error>;

//     #[inline]
//     fn from_request(req: &HttpRequest, _: &mut web::Payload) -> Self::Future {
//         match req.extensions().get::<RequestHub>() {
//             Some(hub) => Ok(hub.clone()),
//             None => Err(error::ErrorInternalServerError(
//                 "Sentry hub is not configured, use Sentry middleware",
//             )),
//         }
//     }
// }

#[derive(Debug)]
pub struct SentryFuture<F> {
    hub: Arc<Hub>,
    future: F,
}

impl<F> SentryFuture<F> {
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

pub trait SentryFutureExt: Sized {
    fn bind_hub<H>(self, hub: H) -> SentryFuture<Self>
    where
        H: Into<Arc<Hub>>,
    {
        SentryFuture {
            future: self,
            hub: hub.into(),
        }
    }

    // TODO(ja): Remove this
    fn sentry_hub_current(self) -> SentryFuture<Self> {
        self.bind_hub(Hub::current())
    }

    // TODO(ja): Remove this
    fn sentry_hub_new_from_current(self) -> SentryFuture<Self> {
        self.bind_hub(Arc::new(Hub::new_from_top(Hub::current())))
    }
}

impl<F> SentryFutureExt for F {}

/// Write own data to Sentry scope, only the subset that is considered useful for debugging. Right
/// now this could've been a simple method, but the idea is that one day we want a custom derive
/// for this.
pub trait ToSentryScope {
    fn to_scope(&self, scope: &mut Scope);

    fn configure_scope(&self) {
        sentry::configure_scope(|scope| self.to_scope(scope));
    }

    fn configure_hub(&self, hub: &Hub) {
        hub.configure_scope(|scope| self.to_scope(scope));
    }
}

// TODO(ja): Make this a thing for all functions
// TODO(ja): Change configure_scope to take any C: ConfigureScope.
