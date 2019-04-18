use std::sync::Arc;

use actix::fut::ActorFuture;
use actix::{Actor, Message};
use futures::future::Future;
use futures::Poll;

use sentry::Hub;

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

impl<F> ActorFuture for SentryFuture<F>
where
    F: ActorFuture,
{
    type Actor = F::Actor;
    type Item = F::Item;
    type Error = F::Error;

    fn poll(
        &mut self,
        srv: &mut Self::Actor,
        ctx: &mut <Self::Actor as Actor>::Context,
    ) -> Poll<Self::Item, Self::Error> {
        Hub::run(self.hub.clone(), || self.inner.poll(srv, ctx))
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

// In order to get the extension trait for both actor futures and regular futures, we just
// implement it for everything. If you call `sentry_hub` on random non-future things you just get a
// useless type.
impl<F> SentryFutureExt for F {}

impl<M> Message for SentryFuture<M>
where
    M: Message,
{
    type Result = M::Result;
}

#[macro_export]
macro_rules! handle_sentry_actix_message {
    ($actor:ident, $message:ident) => { handle_sentry_actix_message!(<>, $actor <>, $message <>); };

    (
        < $($param:ident : $param_req:ident),* >,
        $actor:ident < $($param2:ident),* >,
        $message:ident < $($param3:ident),* >
    ) => {
        impl<$($param : $param_req),*> actix::Handler<crate::sentry::SentryFuture<$message<$($param3),*>>> for $actor <$($param2),*> {
            type Result = <$actor <$($param2),*> as actix::Handler<$message<$($param3),*>>>::Result;

            fn handle(&mut self, request: crate::sentry::SentryFuture<$message<$($param3),*>>, ctx: &mut Self::Context) -> Self::Result {
                ::sentry::Hub::run(request.hub.clone(), || self.handle(request.inner, ctx))
            }
        }
    }
}
