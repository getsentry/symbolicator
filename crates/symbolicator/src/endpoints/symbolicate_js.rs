use std::sync::Arc;

use axum::extract;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use symbolicator_js::interface::{JsModule, JsStacktrace, SymbolicateJsStacktraces};
use symbolicator_service::types::Platform;
use symbolicator_sources::SentrySourceConfig;

use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::service::{RequestService, ScrapingConfig, SymbolicationResponse};
use crate::utils::sentry::ConfigureScope;

use super::ResponseError;

#[derive(Serialize, Deserialize)]
pub struct JsSymbolicationRequestBody {
    pub platform: Option<Platform>,
    pub source: SentrySourceConfig,
    #[serde(default)]
    pub stacktraces: Vec<JsStacktrace>,
    #[serde(default)]
    pub modules: Vec<JsModule>,
    #[serde(default)]
    pub release: Option<String>,
    #[serde(default)]
    pub dist: Option<String>,
    // This is kept around for backwards compatibility.
    // For now, it overrides the `enabled` flag in the `scraping` field.
    #[serde(default = "default_allow_scraping")]
    pub allow_scraping: bool,
    #[serde(default)]
    pub scraping: ScrapingConfig,
    #[serde(default)]
    pub options: JsRequestOptions,
}

fn default_allow_scraping() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
pub struct JsRequestOptions {
    /// Whether to apply source context for the stack frames.
    #[serde(default = "default_apply_source_context")]
    pub apply_source_context: bool,
}

fn default_apply_source_context() -> bool {
    true
}

impl Default for JsRequestOptions {
    fn default() -> Self {
        Self {
            apply_source_context: true,
        }
    }
}

pub async fn handle_symbolication_request(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    extract::Json(body): extract::Json<JsSymbolicationRequestBody>,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let JsSymbolicationRequestBody {
        platform,
        source,
        stacktraces,
        modules,
        release,
        dist,
        allow_scraping,
        mut scraping,
        options,
    } = body;

    // Turn off scraping if `allow_scraping` is false
    scraping.enabled &= allow_scraping;

    let request_id = service.symbolicate_js_stacktraces(SymbolicateJsStacktraces {
        platform,
        scope: params.scope,
        source: Arc::new(source),
        stacktraces,
        modules,
        release,
        dist,
        scraping,
        apply_source_context: options.apply_source_context,
    })?;

    match service.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}
