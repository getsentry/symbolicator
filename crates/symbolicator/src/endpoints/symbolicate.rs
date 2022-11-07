use axum::extract;
use axum::response::Json;
use serde::Deserialize;

use symbolicator_sources::SourceConfig;

use crate::service::{
    RawObjectInfo, RawStacktrace, RequestOptions, RequestService, Scope, Signal, StacktraceOrigin,
    SymbolicateStacktraces, SymbolicationResponse,
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
    extract::Extension(service): extract::Extension<RequestService>,
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
        None => service.config().default_sources(),
    };

    let request_id = service.symbolicate_stacktraces(SymbolicateStacktraces {
        scope: params.scope,
        signal: body.signal,
        sources,
        origin: StacktraceOrigin::Symbolicate,
        stacktraces: body.stacktraces,
        modules: body.modules.into_iter().map(From::from).collect(),
        options: body.options,
    })?;

    match service.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}
