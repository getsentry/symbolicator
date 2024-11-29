use std::sync::Arc;

use axum::{extract, Json};
use serde::{Deserialize, Serialize};
use symbolicator_proguard::interface::{
    JvmException, JvmModule, JvmStacktrace, SymbolicateJvmStacktraces,
};
use symbolicator_service::types::Platform;
use symbolicator_sources::SourceConfig;

use crate::service::{RequestService, SymbolicationResponse};
use crate::utils::sentry::ConfigureScope;

use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::endpoints::ResponseError;

#[derive(Serialize, Deserialize)]
pub struct JvmSymbolicationRequestBody {
    pub platform: Option<Platform>,
    pub sources: Arc<[SourceConfig]>,
    #[serde(default)]
    pub exceptions: Vec<JvmException>,
    #[serde(default)]
    pub stacktraces: Vec<JvmStacktrace>,
    #[serde(default)]
    pub modules: Vec<JvmModule>,
    #[serde(default)]
    pub release_package: Option<String>,
    #[serde(default)]
    pub classes: Vec<Arc<str>>,
    #[serde(default)]
    pub options: JvmRequestOptions,
}

#[derive(Serialize, Deserialize)]
pub struct JvmRequestOptions {
    /// Whether to apply source context for the stack frames.
    #[serde(default = "default_apply_source_context")]
    pub apply_source_context: bool,
}

fn default_apply_source_context() -> bool {
    true
}

impl Default for JvmRequestOptions {
    fn default() -> Self {
        Self {
            apply_source_context: true,
        }
    }
}

pub async fn handle_symbolication_request(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    extract::Json(body): extract::Json<JvmSymbolicationRequestBody>,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let JvmSymbolicationRequestBody {
        platform,
        sources,
        exceptions,
        stacktraces,
        modules,
        release_package,
        classes,
        options,
    } = body;

    let request_id = service.symbolicate_jvm_stacktraces(SymbolicateJvmStacktraces {
        platform,
        scope: params.scope,
        sources,
        exceptions,
        stacktraces,
        modules,
        release_package,
        classes,
        apply_source_context: options.apply_source_context,
    })?;

    match service.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}
