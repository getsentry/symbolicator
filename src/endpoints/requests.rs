use actix_web::{http::Method, HttpResponse, Path, Query, State};
use failure::Error;
use futures::{FutureExt, TryFutureExt};
use futures01::Future;
use serde::Deserialize;

use crate::app::{ServiceApp, ServiceState};
use crate::types::RequestId;
use crate::utils::futures::ResponseFuture;

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

    let future = state
        .symbolication()
        .get_response(path.request_id, query.timeout)
        .never_error()
        .boxed_local()
        .compat()
        .map(|response_opt| match response_opt {
            Some(response) => HttpResponse::Ok().json(response),
            None => HttpResponse::NotFound().finish(),
        })
        .map_err(|never| match never {});

    Box::new(future)
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/requests/{request_id}", |r| {
        r.method(Method::GET).with(poll_request);
    })
}
