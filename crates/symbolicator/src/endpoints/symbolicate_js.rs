use std::sync::Arc;

use axum::extract;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use symbolicator_service::services::symbolication::SymbolicateJsStacktraces;
use symbolicator_service::types::RawObjectInfo;
use symbolicator_sources::SentrySourceConfig;

use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::service::{JsStacktrace, RequestService, SymbolicationResponse};
use crate::utils::sentry::ConfigureScope;

use super::ResponseError;

fn default_allow_scraping() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
pub struct JsSymbolicationRequestBody {
    #[serde(default)]
    pub source: Option<SentrySourceConfig>,
    #[serde(default)]
    pub stacktraces: Vec<JsStacktrace>,
    #[serde(default)]
    pub modules: Vec<RawObjectInfo>,
    #[serde(default)]
    pub release: Option<String>,
    #[serde(default)]
    pub dist: Option<String>,
    #[serde(default = "default_allow_scraping")]
    pub allow_scraping: bool,
}

pub async fn handle_symbolication_request(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    extract::Json(body): extract::Json<JsSymbolicationRequestBody>,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let JsSymbolicationRequestBody {
        source,
        stacktraces,
        modules,
        release,
        dist,
        allow_scraping,
    } = body;

    let request_id = service.symbolicate_js_stacktraces(SymbolicateJsStacktraces {
        scope: params.scope,
        source: Arc::new(source.unwrap()),
        stacktraces,
        modules,
        release,
        dist,
        allow_scraping,
    })?;

    match service.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}
