use std::sync::Arc;

use axum::extract;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use symbolicator_service::services::symbolication::JsProcessingSymbolicateStacktraces;
use symbolicator_sources::SentrySourceConfig;

use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::service::{JsProcessingStacktrace, RequestService, SymbolicationResponse};
use crate::utils::sentry::ConfigureScope;

use super::ResponseError;

#[derive(Serialize, Deserialize)]
pub struct SourcemapRequestBody {
    #[serde(default)]
    pub source: Option<SentrySourceConfig>,
    #[serde(default)]
    pub stacktraces: Vec<JsProcessingStacktrace>,
    #[serde(default)]
    pub dist: Option<String>,
}

pub async fn handle_sourcemap_request(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    extract::Json(body): extract::Json<SourcemapRequestBody>,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let SourcemapRequestBody {
        source,
        stacktraces,
        dist,
    } = body;

    let request_id =
        service.js_processing_symbolicate_stacktraces(JsProcessingSymbolicateStacktraces {
            source: Arc::new(source.unwrap()),
            stacktraces,
            dist,
        })?;

    match service.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}
