use std::sync::Arc;

use actix_web::{http::Method, HttpRequest, Json, Query, State};
use failure::Error;
use futures::{FutureExt, TryFutureExt};
use futures01::Future;
use sentry::{configure_scope, Hub};
use serde::Deserialize;

use crate::actors::symbolication::SymbolicateStacktraces;
use crate::app::{ServiceApp, ServiceState};
use crate::sources::SourceConfig;
use crate::types::{
    RawObjectInfo, RawStacktrace, RequestOptions, Scope, Signal, SymbolicationResponse,
};
use crate::utils::futures::ResponseFuture;
use crate::utils::sentry::{ActixWebHubExt, SentryFutureExt, WriteSentryScope};

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

fn symbolicate_frames(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    body: Json<SymbolicationRequestBody>,
    request: HttpRequest<ServiceState>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    let hub = Hub::from_request(&request);
    hub.start_session();

    Hub::run(hub, || {
        let params = params.into_inner();
        let body = body.into_inner();
        let sources = match body.sources {
            Some(sources) => Arc::from(sources),
            None => state.config().default_sources(),
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
            options: body.options,
        };

        let request_id = state.symbolication().symbolicate_stacktraces(message);

        let timeout = params.timeout;
        let response = state
            .symbolication()
            .get_response(request_id, timeout)
            .never_error()
            .boxed_local()
            .compat()
            .map(|x| Json(x.expect("Race condition: Inserted request not found!")))
            .map_err(|never| match never {});

        Box::new(response.sentry_hub_current())
    })
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/symbolicate", |r| {
        r.method(Method::POST).with_config(
            symbolicate_frames,
            |(_state, _params, body, _request)| {
                body.limit(5_000_000);
            },
        );
    })
}
