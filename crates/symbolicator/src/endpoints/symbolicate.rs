use axum::extract;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use symbolicator_native::interface::{
    RawStacktrace, Signal, StacktraceOrigin, SymbolicateStacktraces,
};
use symbolicator_service::types::{Platform, RawObjectInfo};
use symbolicator_sources::SourceConfig;

use crate::service::{
    RequestOptions, RequestService, Scope, ScrapingConfig, SymbolicationResponse,
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
#[derive(Serialize, Deserialize)]
pub struct SymbolicationRequestBody {
    pub platform: Option<Platform>,
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
    #[serde(default)]
    pub scraping: ScrapingConfig,
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
            platform: body.platform,
            scope: params.scope,
            signal: body.signal,
            sources,
            origin: StacktraceOrigin::Symbolicate,
            stacktraces: body.stacktraces,
            modules: body.modules.into_iter().map(From::from).collect(),
            apply_source_context: body.options.apply_source_context,
            scraping: body.scraping,
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
    use super::*;

    use reqwest::{Client, StatusCode};
    use symbolicator_native::interface::CompletedSymbolicationResponse;

    use crate::test;

    #[tokio::test]
    async fn test_body_limit() {
        test::setup();

        let server = test::server_with_default_service();

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

    #[tokio::test]
    async fn test_unknown_field() {
        test::setup();

        let server = test::server_with_default_service();

        let payload = r#"{
            "stacktraces": [],
            "modules": [],
            "sources": [],
            "unknown": "value"
        }"#;

        let response = Client::new()
            .post(server.url("/symbolicate"))
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Asserts that disabling requesting for DIF candidates info works.
    #[tokio::test]
    async fn test_no_dif_candidates() {
        test::setup();

        let server = test::server_with_default_service();

        let payload = r#"{
            "stacktraces": [{
              "registers": {"eip": "0x0000000001509530"},
              "frames": [{"instruction_addr": "0x749d37f2"}]
            }],
            "modules": [{
              "type": "pe",
              "debug_id": "3249d99d-0c40-4931-8610-f4e4fb0b6936-1",
              "code_file": "C:\\Windows\\System32\\kernel32.dll",
              "debug_file": "C:\\Windows\\System32\\crash.pdb",
              "image_addr": "0x749d0000",
              "image_size": 851968
            }],
            "sources": []
        }"#;
        let mut payload: SymbolicationRequestBody = serde_json::from_str(payload).unwrap();

        let (_srv, source) = test::symbol_server();
        payload.sources = Some(vec![source]);

        let response = Client::new()
            .post(server.url("/symbolicate"))
            .json(&payload)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // TODO: assert symbol server hits as side-effect?

        let response: CompletedSymbolicationResponse = response.json().await.unwrap();
        test::assert_snapshot!(response);
    }

    /// Requests could contain invalid data which should not stop symbolication,
    /// name is sent by Sentry and is unknown.
    #[tokio::test]
    async fn test_unknown_source_config() {
        test::setup();

        let server = test::server_with_default_service();

        let payload = r#"{
            "stacktraces": [{
              "registers": {"eip": "0x0000000001509530"},
              "frames": [{"instruction_addr": "0x749e8630"}]
            }],
            "modules": [{
              "type": "pe",
              "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
              "code_file": "C:\\Windows\\System32\\kernel32.dll",
              "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
              "image_addr": "0x749d0000",
              "image_size": 851968
            }],
            "sources": [{
              "type": "http",
              "id": "unknown",
              "layout": {"type": "symstore"},
              "filters": {"filetypes": ["mach_code"]},
              "url": "http://notfound",
              "name": "not a known field",
              "not-a-field": "more unknown fields"
            }],
            "options": {"dif_candidates": true}
        }"#;

        let response = Client::new()
            .post(server.url("/symbolicate"))
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let response: CompletedSymbolicationResponse = response.json().await.unwrap();
        test::assert_snapshot!(response);
    }
}
