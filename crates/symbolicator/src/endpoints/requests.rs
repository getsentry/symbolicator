use axum::extract;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::service::{RequestId, RequestService, SymbolicationResponse};

/// Query parameters of the symbolication poll request.
#[derive(Deserialize)]
pub struct PollSymbolicationRequestQueryParams {
    #[serde(default)]
    pub timeout: Option<u64>,
}

pub async fn poll_request(
    extract::State(service): extract::State<RequestService>,
    extract::Path(request_id): extract::Path<RequestId>,
    extract::Query(query): extract::Query<PollSymbolicationRequestQueryParams>,
) -> Result<Json<SymbolicationResponse>, StatusCode> {
    sentry::configure_scope(|scope| {
        scope.set_transaction(Some("GET /requests"));
    });

    let response_opt = service.get_response(request_id, query.timeout).await;

    match response_opt {
        Some(response) => Ok(Json(response)),
        None => Err(StatusCode::NOT_FOUND),
    }
}
