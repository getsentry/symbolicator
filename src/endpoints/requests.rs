use actix::ResponseFuture;
use actix_web::{http::Method, HttpResponse, Path, Query, State};
use failure::Error;
use futures::Future;
use serde::Deserialize;

use crate::actors::symbolication::GetSymbolicationStatus;
use crate::app::{ServiceApp, ServiceState};
use crate::types::{RequestId, SymbolicationError};

/// Path parameters of the symbolication poll request.
#[derive(Deserialize)]
struct PollSymbolicationRequestPath {
    pub request_id: RequestId,
}

/// Query parameters of the symbolication poll request.
#[derive(Deserialize)]
struct PollSymbolicationRequestQueryParams {
    #[serde(default)]
    pub timeout: Option<u64>,
}

fn poll_request(
    state: State<ServiceState>,
    path: Path<PollSymbolicationRequestPath>,
    query: Query<PollSymbolicationRequestQueryParams>,
) -> ResponseFuture<HttpResponse, Error> {
    let path = path.into_inner();
    let query = query.into_inner();

    let message = GetSymbolicationStatus {
        request_id: path.request_id,
        timeout: query.timeout,
    };

    let future = state
        .symbolication
        .send(message)
        .map_err(|_| SymbolicationError::Mailbox)
        .flatten()
        .map(|response_opt| match response_opt {
            Some(response) => HttpResponse::Ok().json(response),
            None => HttpResponse::NotFound().finish(),
        })
        .map_err(Error::from);

    Box::new(future)
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/requests/{request_id}", |r| {
        r.method(Method::GET).with(poll_request);
    })
}
