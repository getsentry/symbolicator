use axum::extract;
use axum::response::Json;
use serde::Deserialize;

use crate::services::symbolication::{StacktraceOrigin, SymbolicateStacktraces};
use crate::services::Service;
use crate::sources::SourceConfig;
use crate::types::{
    RawObjectInfo, RawStacktrace, RequestOptions, Scope, Signal, SymbolicationResponse,
};
use crate::utils::sentry::ConfigureScope;

use super::ResponseError;

/// Query parameters of the symbolication request.
#[derive(Deserialize)]
pub struct SymbolicationRequestQueryParams {
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub scope: Scope,
}

impl ConfigureScope for SymbolicationRequestQueryParams {
    fn to_scope(&self, scope: &mut sentry::Scope) {
        scope.set_tag("request.scope", &self.scope);
        if let Some(timeout) = self.timeout {
            scope.set_tag("request.timeout", timeout);
        } else {
            scope.set_tag("request.timeout", "none");
        }
    }
}

/// JSON body of the symbolication request.
#[derive(Deserialize)]
pub struct SymbolicationRequestBody {
    #[serde(default)]
    pub signal: Option<Signal>,
    #[serde(default)]
    pub sources: Option<Vec<SourceConfig>>,
    #[serde(default)]
    pub stacktraces: Vec<RawStacktrace>,
    #[serde(default)]
    pub modules: Vec<RawObjectInfo>,
    #[serde(default)]
    pub options: RequestOptions,
}

pub async fn symbolicate_frames(
    extract::Extension(state): extract::Extension<Service>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    extract::ContentLengthLimit(extract::Json(body)): extract::ContentLengthLimit<
        extract::Json<SymbolicationRequestBody>,
        { 5 * 1024 * 1024 }, // ~5MB
    >,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let sources = match body.sources {
        Some(sources) => sources.into(),
        None => state.config().default_sources(),
    };

    let symbolication = state.symbolication();
    let request_id = symbolication.symbolicate_stacktraces(SymbolicateStacktraces {
        scope: params.scope,
        signal: body.signal,
        sources,
        origin: StacktraceOrigin::Symbolicate,
        stacktraces: body.stacktraces,
        modules: body.modules.into_iter().map(From::from).collect(),
        options: body.options,
    })?;

    match symbolication.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}
