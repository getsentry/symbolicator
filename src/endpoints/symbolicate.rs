use actix::ResponseFuture;
use actix_web::{http::Method, Json, Query, State};
use failure::Error;
use futures::Future;
use serde::Deserialize;

use crate::actors::symbolication::{GetSymbolicationStatus, SymbolicateStacktraces};
use crate::app::{ServiceApp, ServiceState};
use crate::sentry::SentryFutureExt;
use crate::types::{
    ObjectInfo, RawStacktrace, Scope, Signal, SourceConfig, SymbolicationError,
    SymbolicationResponse,
};

/// Query parameters of the symbolication request.
#[derive(Deserialize)]
pub struct SymbolicationRequestQueryParams {
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub scope: Scope,
}

/// JSON body of the symbolication request.
#[derive(Deserialize)]
struct SymbolicationRequestBody {
    #[serde(default)]
    pub signal: Option<Signal>,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub stacktraces: Vec<RawStacktrace>,
    #[serde(default)]
    pub modules: Vec<ObjectInfo>,
}

fn symbolicate_frames(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    body: Json<SymbolicationRequestBody>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    let params = params.into_inner();
    let body = body.into_inner();

    let message = SymbolicateStacktraces {
        signal: body.signal,
        sources: body.sources,
        stacktraces: body.stacktraces,
        modules: body.modules,
        scope: params.scope,
    }
    .sentry_hub_new_from_current();

    let request_id = state
        .symbolication
        .send(message)
        .map_err(|_| SymbolicationError::Mailbox)
        .flatten();

    let timeout = params.timeout;
    let response = request_id
        .and_then(move |request_id| {
            state
                .symbolication
                .send(
                    GetSymbolicationStatus {
                        request_id,
                        timeout,
                    }
                    .sentry_hub_current(),
                )
                .map_err(|_| SymbolicationError::Mailbox)
        })
        .flatten()
        .and_then(|response_opt| response_opt.ok_or(SymbolicationError::Mailbox))
        .map(Json)
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
