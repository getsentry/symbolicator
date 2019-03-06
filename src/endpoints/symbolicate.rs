use actix::ResponseFuture;

use futures::future::Future;

use actix_web::{http::Method, Json, State};

use failure::Error;

use crate::{
    app::{ServiceApp, ServiceState},
    types::{SymbolicateFramesRequest, SymbolicateFramesResponse, SymbolicationError},
};

fn symbolicate_frames(
    state: State<ServiceState>,
    request: Json<SymbolicateFramesRequest>,
) -> ResponseFuture<Json<SymbolicateFramesResponse>, Error> {
    Box::new(
        state
            .symbolication
            .send(request.into_inner())
            .map_err(SymbolicationError::from)
            .flatten()
            .map(Json)
            .map_err(From::from),
    )
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/symbolicate", |r| {
        r.method(Method::POST).with(symbolicate_frames);
    })
}
