use axum::{
    Json, extract,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{
    endpoints::{ResponseError, tools},
    service::{RequestQueryParams, RequestService},
};

/// An enum of all possible tools invocations for Symbolicator.
#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum ToolRequestKind {
    GetMetadata(tools::get_metadata::Request),
}

/// A wrapper for Symbolicator tool invocations, containing common data.
#[derive(Deserialize)]
pub struct ToolRequest {
    request_kind: ToolRequestKind,
}

/// The entrypoint for all tool invocations.
pub async fn invoke_tool(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<RequestQueryParams>,
    extract::Json(body): extract::Json<ToolRequest>,
) -> Result<Response, ResponseError> {
    match body.request_kind {
        ToolRequestKind::GetMetadata(tools::get_metadata::Request {
            minidump_or_report,
            sources,
            platform,
            storage_id,
        }) => tools::get_metadata::invoke(
            service,
            params,
            platform,
            minidump_or_report,
            sources,
            storage_id,
        )
        .await
        .map(|v| IntoResponse::into_response(Json(v))),
    }
}
