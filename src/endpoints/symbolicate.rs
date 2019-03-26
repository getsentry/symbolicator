use actix::ResponseFuture;

use actix_web::{http::Method, Json, State};

use failure::{Error, Fail};

use futures::future::Future;

use crate::{
    app::{ServiceApp, ServiceState},
    types::{
        SymbolicationRequest, SymbolicationResponse, SymbolicationError,
        SymbolicationErrorKind,
    },
};

fn symbolicate_frames(
    state: State<ServiceState>,
    request: Json<SymbolicationRequest>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    Box::new(
        state
            .symbolication
            .send(request.into_inner())
            .map_err(|e| e.context(SymbolicationErrorKind::Mailbox))
            .map_err(SymbolicationError::from)
            .flatten()
            .map(Json)
            .map_err(Error::from),
    )
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/symbolicate", |r| {
        r.method(Method::POST).with(symbolicate_frames);
    })
}
