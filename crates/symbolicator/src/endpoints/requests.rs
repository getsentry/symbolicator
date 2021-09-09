use axum::extract;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::services::Service;
use crate::types::{RequestId, SymbolicationResponse};

/// Query parameters of the symbolication poll request.
#[derive(Deserialize)]
pub struct PollSymbolicationRequestQueryParams {
    #[serde(default)]
    pub timeout: Option<u64>,
}

pub async fn poll_request(
    extract::Extension(state): extract::Extension<Service>,
    extract::Path(request_id): extract::Path<RequestId>,
    extract::Query(query): extract::Query<PollSymbolicationRequestQueryParams>,
) -> Result<Json<SymbolicationResponse>, StatusCode> {
    let response_opt = state
        .symbolication()
        .get_response(request_id, query.timeout)
        .await;

    match response_opt {
        Some(response) => Ok(Json(response)),
        None => Err(StatusCode::NOT_FOUND),
    }
}
