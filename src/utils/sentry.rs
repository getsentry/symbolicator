#![allow(deprecated)]

use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use actix_web::middleware::{Finished, Middleware, Response, Started};
use actix_web::{Error, FromRequest, HttpRequest, HttpResponse};
use failure::Fail;
use futures01::future::Future;
use futures01::Poll;
use sentry::integrations::backtrace::parse_stacktrace;
use sentry::protocol::{ClientSdkPackage, Event, Exception};
use sentry::{parse_type_from_debug, Hub, Level, Scope, ScopeGuard};
use uuid::Uuid;

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
        self.bind_hub(Hub::new_from_top(Hub::current()))
    }
}

impl<F> SentryFutureExt for F where F: futures01::future::Future {}

/// Write own data to [`sentry::Scope`], only the subset that is considered useful for debugging.
pub trait ConfigureScope {
    /// Writes information to the given scope.
    fn to_scope(&self, scope: &mut Scope);

    /// Configures the current scope.
    fn configure_scope(&self) {
        sentry::configure_scope(|scope| self.to_scope(scope));
    }
}

/// Reports certain failures to sentry.
pub struct SentryMiddleware {
    emit_header: bool,
    capture_server_errors: bool,
}

struct HubWrapper {
    hub: Arc<Hub>,
    root_scope: RefCell<Option<ScopeGuard>>,
}

impl SentryMiddleware {
    /// Creates a new sentry middleware.
    pub fn new() -> SentryMiddleware {
        SentryMiddleware {
            emit_header: false,
            capture_server_errors: true,
        }
    }

    fn new_hub(&self) -> Arc<Hub> {
        Arc::new(Hub::new_from_top(Hub::main()))
    }
}

impl Default for SentryMiddleware {
    fn default() -> Self {
        SentryMiddleware::new()
    }
}

fn extract_request<S: 'static>(
    req: &HttpRequest<S>,
    with_pii: bool,
) -> (Option<String>, sentry::protocol::Request) {
    let resource = req.resource();
    let transaction = if let Some(rdef) = resource.rdef() {
        Some(rdef.pattern().to_string())
    } else if resource.name() != "" {
        Some(resource.name().to_string())
    } else {
        None
    };
    let mut sentry_req = sentry::protocol::Request {
        url: format!(
            "{}://{}{}",
            req.connection_info().scheme(),
            req.connection_info().host(),
            req.uri()
        )
        .parse()
        .ok(),
        method: Some(req.method().to_string()),
        headers: req
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().into(), v.to_str().unwrap_or("").into()))
            .collect(),
        ..Default::default()
    };

    if with_pii {
        if let Some(remote) = req.connection_info().remote() {
            sentry_req.env.insert("REMOTE_ADDR".into(), remote.into());
        }
    };

    (transaction, sentry_req)
}

impl<S: 'static> Middleware<S> for SentryMiddleware {
    fn start(&self, req: &HttpRequest<S>) -> Result<Started, Error> {
        let hub = self.new_hub();
        let outer_req = req;
        let req = outer_req.clone();
        let client = hub.client();

        let req = fragile::SemiSticky::new(req);
        let cached_data = Arc::new(Mutex::new(None));

        let root_scope = hub.push_scope();
        hub.configure_scope(move |scope| {
            scope.add_event_processor(Box::new(move |mut event| {
                let mut cached_data = cached_data.lock().unwrap();
                if cached_data.is_none() && req.is_valid() {
                    let with_pii = client
                        .as_ref()
                        .map_or(false, |x| x.options().send_default_pii);
                    *cached_data = Some(extract_request(&req.get(), with_pii));
                }

                if let Some((ref transaction, ref req)) = *cached_data {
                    if event.transaction.is_none() {
                        event.transaction = transaction.clone();
                    }
                    if event.request.is_none() {
                        event.request = Some(req.clone());
                    }
                }

                if let Some(sdk) = event.sdk.take() {
                    let mut sdk = sdk.into_owned();
                    sdk.packages.push(ClientSdkPackage {
                        name: "sentry-actix".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    });
                    event.sdk = Some(Cow::Owned(sdk));
                }

                Some(event)
            }));
        });

        outer_req.extensions_mut().insert(HubWrapper {
            hub,
            root_scope: RefCell::new(Some(root_scope)),
        });
        Ok(Started::Done)
    }

    fn response(&self, req: &HttpRequest<S>, mut resp: HttpResponse) -> Result<Response, Error> {
        if self.capture_server_errors && resp.status().is_server_error() {
            let event_id = if let Some(error) = resp.error() {
                Some(Hub::from_request(req).capture_actix_error(error))
            } else {
                None
            };
            match event_id {
                Some(event_id) if self.emit_header => {
                    resp.headers_mut().insert(
                        "x-sentry-event",
                        event_id.to_simple_ref().to_string().parse().unwrap(),
                    );
                }
                _ => {}
            }
        }
        Ok(Response::Done(resp))
    }

    fn finish(&self, req: &HttpRequest<S>, _resp: &HttpResponse) -> Finished {
        // if we make it to the end of the request we want to first drop the root
        // scope before we drop the entire hub.  This will first drop the closures
        // on the scope which in turn will release the circular dependency we have
        // with the hub via the request.
        if let Some(hub_wrapper) = req.extensions().get::<HubWrapper>() {
            if let Ok(mut guard) = hub_wrapper.root_scope.try_borrow_mut() {
                guard.take();
            }
        }
        Finished::Done
    }
}

/// Hub extensions for [`actix_web`].
pub trait ActixWebHubExt {
    /// Returns the hub from a given http request.
    ///
    /// This requires that [`SentryMiddleware`] has been enabled or the call will panic.
    fn from_request<S>(req: &HttpRequest<S>) -> Arc<Hub>;
    /// Captures an actix error on the given hub.
    fn capture_actix_error(&self, err: &Error) -> Uuid;
}

impl ActixWebHubExt for Hub {
    fn from_request<S>(req: &HttpRequest<S>) -> Arc<Hub> {
        req.extensions()
            .get::<HubWrapper>()
            .expect("SentryMiddleware middleware was not registered")
            .hub
            .clone()
    }

    fn capture_actix_error(&self, err: &Error) -> Uuid {
        let mut exceptions = vec![];
        let mut ptr: Option<&dyn Fail> = Some(err.as_fail());
        let mut idx = 0;
        while let Some(fail) = ptr {
            // Check whether the failure::Fail held by err is a failure::Error wrapped in Compat
            // If that's the case, we should be logging that error and its fail instead of the wrapper's construction in actix_web
            // This wouldn't be necessary if failure::Compat<failure::Error>'s Fail::backtrace() impl was not "|| None",
            // that is however impossible to do as of now because it conflicts with the generic implementation of Fail also provided in failure.
            // Waiting for update that allows overlap, (https://github.com/rust-lang/rfcs/issues/1053), but chances are by then failure/std::error will be refactored anyway
            let compat: Option<&failure::Compat<failure::Error>> = fail.downcast_ref();
            let failure_err = compat.map(failure::Compat::get_ref);
            let fail = failure_err.map_or(fail, |x| x.as_fail());
            exceptions.push(exception_from_single_fail(
                fail,
                if idx == 0 {
                    Some(failure_err.map_or_else(|| err.backtrace(), |err| err.backtrace()))
                } else {
                    fail.backtrace()
                },
            ));
            ptr = fail.cause();
            idx += 1;
        }
        exceptions.reverse();
        self.capture_event(Event {
            exception: exceptions.into(),
            level: Level::Error,
            ..Default::default()
        })
    }
}

fn exception_from_single_fail<F: Fail + ?Sized>(
    f: &F,
    bt: Option<&failure::Backtrace>,
) -> Exception {
    let dbg = format!("{:?}", f);
    Exception {
        ty: parse_type_from_debug(&dbg).to_owned(),
        value: Some(f.to_string()),
        stacktrace: bt
            // format the stack trace with alternate debug to get addresses
            .map(|bt| format!("{:#?}", bt))
            .and_then(|x| parse_stacktrace(&x)),
        ..Default::default()
    }
}

#[derive(Debug)]
pub struct ActixHub(Arc<Hub>);

impl<S> FromRequest<S> for ActixHub {
    type Config = ();
    type Result = Self;

    fn from_request(req: &HttpRequest<S>, _: &Self::Config) -> Self::Result {
        Self(Hub::from_request(req))
    }
}

impl From<ActixHub> for Arc<Hub> {
    fn from(ah: ActixHub) -> Self {
        ah.0
    }
}
