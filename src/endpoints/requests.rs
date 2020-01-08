use actix_web::{HttpResponse, Path, Query, State};
use failure::Error;
use serde::Deserialize;

use crate::app::{ServiceApp, ServiceState};
use crate::types::RequestId;

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

async fn poll_request(
    state: State<ServiceState>,
    path: Path<PollSymbolicationRequestPath>,
    query: Query<PollSymbolicationRequestQueryParams>,
) -> Result<HttpResponse, Error> {
    let path = path.into_inner();
    let query = query.into_inner();

    let response_opt = state
        .symbolication()
        .get_response(path.request_id, query.timeout)
        .await?;

    Ok(match response_opt {
        Some(response) => HttpResponse::Ok().json(response),
        None => HttpResponse::NotFound().finish(),
    })
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/requests/{request_id}", |r| {
        let handler = compat_handler!(poll_request, s, p, q);
        r.get().with_async(handler);
    })
}
