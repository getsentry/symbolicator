use actix_web::{Json, Query, State};
use failure::Error;
use sentry::configure_scope;
use serde::Deserialize;

use crate::actors::symbolication::SymbolicateStacktraces;
use crate::app::{ServiceApp, ServiceState};
use crate::types::{
    RawObjectInfo, RawStacktrace, Scope, Signal, SourceConfig, SymbolicationResponse,
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
}

async fn symbolicate_frames(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    body: Json<SymbolicationRequestBody>,
) -> Result<Json<SymbolicationResponse>, Error> {
    let params = params.into_inner();

    configure_scope(|scope| {
        params.write_sentry_scope(scope);
    });

    let body = body.into_inner();
    let sources = match body.sources {
        Some(sources) => sources.into(),
        None => state.config().default_sources(),
    };

    let request_id = state
        .symbolication()
        .symbolicate_stacktraces(SymbolicateStacktraces {
            signal: body.signal,
            sources,
            stacktraces: body.stacktraces,
            modules: body.modules.into_iter().map(From::from).collect(),
            scope: params.scope,
        })?;

    let response_opt = state
        .symbolication()
        .get_response(request_id, params.timeout)
        .await?;

    let response = response_opt.expect("Race condition: Inserted request not found!");
    Ok(Json(response))
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/symbolicate", |r| {
        r.post().with_async_config(
            compat_handler!(symbolicate_frames, s, p, b),
            |(_state, _params, body)| {
                body.limit(5_000_000);
            },
        );
    })
}
