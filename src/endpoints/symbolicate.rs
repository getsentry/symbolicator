use std::sync::Arc;

use actix::ResponseFuture;
use actix_web::{http::Method, Json, Query, State};
use failure::Error;
use futures::Future;
use sentry::configure_scope;
use serde::Deserialize;

use crate::actors::symbolication::{GetSymbolicationStatus, SymbolicateStacktraces};
use crate::app::{ServiceApp, ServiceState};
use crate::sentry::{SentryFutureExt, WriteSentryScope};
use crate::types::{
    RawObjectInfo, RawStacktrace, Scope, Signal, SourceConfig, SymbolicationResponse,
};

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

fn symbolicate_frames(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    body: Json<SymbolicationRequestBody>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    let params = params.into_inner();
    let body = body.into_inner();
    let sources = match body.sources {
        Some(sources) => Arc::new(sources),
        None => state.config.sources.clone(),
    };

    configure_scope(|scope| {
        params.write_sentry_scope(scope);
    });

    let message = SymbolicateStacktraces {
        signal: body.signal,
        sources,
        stacktraces: body.stacktraces,
        modules: body.modules.into_iter().map(From::from).collect(),
        scope: params.scope,
    };

    let request_id = tryf!(state.symbolication.symbolicate_stacktraces(message));

    let timeout = params.timeout;
    let response = state
        .symbolication
        .get_symbolication_status(GetSymbolicationStatus {
            request_id,
            timeout,
        })
        .map(|x| Json(x.expect("Race condition: Inserted request not found!")))
        .map_err(Error::from);

    Box::new(response.sentry_hub_new_from_current())
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/symbolicate", |r| {
        r.method(Method::POST)
            .with_config(symbolicate_frames, |(_state, _params, body)| {
                body.limit(5_000_000);
            });
    })
}
