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
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    extract::Json(body): extract::Json<SymbolicationRequestBody>,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let sources = match body.sources {
        Some(sources) => sources.into(),
        None => service.config().default_sources(),
    };

    let request_id = service.symbolicate_stacktraces(
        SymbolicateStacktraces {
            scope: params.scope,
            signal: body.signal,
            sources,
            origin: StacktraceOrigin::Symbolicate,
            stacktraces: body.stacktraces,
            modules: body.modules.into_iter().map(From::from).collect(),
        },
        body.options,
    )?;

    match service.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}

#[cfg(test)]
mod tests {
    use reqwest::{Client, StatusCode};

    use crate::test;

    #[tokio::test]
    async fn test_body_limit() {
        test::setup();

        let server = test::server_with_default_service().await;

        let mut buf = vec![b'.'; 4 * 1024 * 1024];
        buf[0] = b'"';
        *buf.last_mut().unwrap() = b'"';

        let response = Client::new()
            .post(server.url("/symbolicate"))
            .header("Content-Type", "application/json")
            .body(buf)
            .send()
            .await
            .unwrap();

        // the JSON does not fit our schema :-)
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let mut buf = vec![b'.'; 8 * 1024 * 1024];
        buf[0] = b'"';
        *buf.last_mut().unwrap() = b'"';

        let response = Client::new()
            .post(server.url("/symbolicate"))
            .header("Content-Type", "application/json")
            .body(buf)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
