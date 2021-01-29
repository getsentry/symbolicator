use actix_web::{error, Error, Json, Query, State};
use serde::Deserialize;

use crate::actors::symbolication::SymbolicateStacktraces;
use crate::app::{ServiceApp, ServiceState};
use crate::sources::SourceConfig;
use crate::types::{
    RawObjectInfo, RawStacktrace, RequestOptions, Scope, Signal, SymbolicationResponse,
};
use crate::utils::sentry::WriteSentryScope;

/// Query parameters of the symbolication request.
#[derive(Deserialize)]
pub struct SymbolicationRequestQueryParams {
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub scope: Scope,
}

impl WriteSentryScope for SymbolicationRequestQueryParams {
    fn write_sentry_scope(&self, scope: &mut sentry::Scope) {
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
struct SymbolicationRequestBody {
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

async fn symbolicate_frames(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    body: Json<SymbolicationRequestBody>,
) -> Result<Json<SymbolicationResponse>, Error> {
    sentry::start_session();

    let params = params.into_inner();
    sentry::configure_scope(|scope| params.write_sentry_scope(scope));

    let body = body.into_inner();
    let sources = match body.sources {
        Some(sources) => sources.into(),
        None => state.config().default_sources(),
    };

    let symbolication = state.symbolication();
    let request_id = symbolication.symbolicate_stacktraces(SymbolicateStacktraces {
        scope: params.scope,
        signal: body.signal,
        sources,
        stacktraces: body.stacktraces,
        modules: body.modules.into_iter().map(From::from).collect(),
        options: body.options,
    });

    match symbolication.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err(error::ErrorInternalServerError(
            "symbolication request did not start",
        )),
    }
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/symbolicate", |r| {
        r.post().with_async_config(
            compat_handler!(symbolicate_frames, s, p, b),
            |(_hub, _state, _params, body)| {
                body.limit(5_000_000);
            },
        );
    })
}
