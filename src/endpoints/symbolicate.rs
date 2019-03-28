use actix::ResponseFuture;
use actix_web::{http::Method, Json, Query, State};
use failure::{Error, Fail};
use futures::Future;

use crate::app::{ServiceApp, ServiceState};
use crate::types::{
    RequestMeta, RequestWithMeta, SymbolicationError, SymbolicationErrorKind, SymbolicationRequest,
    SymbolicationResponse,
};

fn symbolicate_frames(
    state: State<ServiceState>,
    request: Json<SymbolicationRequest>,
    meta: Query<RequestMeta>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    Box::new(
        state
            .symbolication
            .send(RequestWithMeta(request.into_inner(), meta.into_inner()))
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
