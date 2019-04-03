use actix::ResponseFuture;
use actix_web::{http::Method, Json, Query, State};
use failure::{Error, Fail};
use futures::Future;
use serde::Deserialize;

use crate::actors::symbolication::{GetSymbolicationStatus, SymbolicateStacktraces};
use crate::app::{ServiceApp, ServiceState};
use crate::types::{
    ObjectInfo, RawStacktrace, Scope, Signal, SourceConfig, SymbolicationError,
    SymbolicationErrorKind, SymbolicationResponse,
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
    };

    let request_id = state
        .symbolication
        .send(message)
        .map_err(|e| e.context(SymbolicationErrorKind::Mailbox))
        .map_err(SymbolicationError::from)
        .flatten();

    let timeout = params.timeout;
    let response = request_id
        .and_then(move |request_id| {
            state
                .symbolication
                .send(GetSymbolicationStatus {
                    request_id,
                    timeout,
                })
                .map_err(|e| e.context(SymbolicationErrorKind::Mailbox))
                .map_err(SymbolicationError::from)
        })
        .flatten()
        .and_then(|response_opt| response_opt.ok_or(SymbolicationErrorKind::Mailbox.into()))
        .map(Json)
        .map_err(Error::from);

    Box::new(response)
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/symbolicate", |r| {
        r.method(Method::POST).with(symbolicate_frames);
    })
}
