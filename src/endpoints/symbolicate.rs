use actix_web::{error, App, Error, Json, Query, State};
use serde::Deserialize;

use crate::services::symbolication::SymbolicateStacktraces;
use crate::services::Service;
use crate::sources::SourceConfig;
use crate::types::{
    RawObjectInfo, RawStacktrace, RequestOptions, Scope, Signal, SymbolicationResponse,
};
use crate::utils::sentry::ConfigureScope;

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
    state: State<Service>,
    params: Query<SymbolicationRequestQueryParams>,
    body: Json<SymbolicationRequestBody>,
) -> Result<Json<SymbolicationResponse>, Error> {
    sentry::start_session();

    let params = params.into_inner();
    params.configure_scope();

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

pub fn configure(app: App<Service>) -> App<Service> {
    app.resource("/symbolicate", |r| {
        r.post().with_async_config(
            compat_handler!(symbolicate_frames, s, p, b),
            |(_hub, _state, _params, body)| {
                body.limit(5_000_000);
            },
        );
    })
}

#[cfg(test)]
mod tests {
    use actix_web::test::TestServer;
    use reqwest::{Client, StatusCode};

    use crate::config::Config;
    use crate::services::Service;
    use crate::test;
    use crate::types::SymbolicationResponse;

    #[tokio::test]
    async fn test_no_sources() {
        test::setup();

        let service = Service::create(Config::default()).unwrap();
        let server = TestServer::with_factory(move || crate::server::create_app(service.clone()));

        let payload = r##"{
            "stacktraces": [
                {
                    "registers": {"eip": "0x0000000001509530"},
                    "frames": [{"instruction_addr": "0x749e8630"}]
                }
            ],
            "modules": [
                {
                    "type": "pe",
                    "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
                    "code_file": "C:\\Windows\\System32\\kernel32.dll",
                    "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
                    "image_addr": "0x749d0000",
                    "image_size": 851968
                }
            ],
            "sources": []
        }"##;

        let response = Client::new()
            .post(&server.url("/symbolicate"))
            .header("content-type", "application/json")
            .body(payload)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.text().await.unwrap();
        let response = serde_json::from_str::<SymbolicationResponse>(&body).unwrap();
        insta::assert_yaml_snapshot!(response);
    }

    #[tokio::test]
    async fn test_unknown_field() {
        test::setup();

        let service = Service::create(Config::default()).unwrap();
        let server = TestServer::with_factory(move || crate::server::create_app(service.clone()));

        let payload = r##"{
            "stacktraces": [
                {
                    "registers": {"eip": "0x0000000001509530"},
                    "frames": [{"instruction_addr": "0x749e8630"}]
                }
            ],
            "modules": [
                {
                    "type": "pe",
                    "debug_id": "ff9f9f78-41db-88f0-cded-a9e1e9bff3b5-1",
                    "code_file": "C:\\Windows\\System32\\kernel32.dll",
                    "debug_file": "C:\\Windows\\System32\\wkernel32.pdb",
                    "image_addr": "0x749d0000",
                    "image_size": 851968
                }
            ],
            "sources": [],
            "unknown": "value"
        }"##;

        let response = Client::new()
            .post(&server.url("/symbolicate"))
            .header("content-type", "application/json")
            .body(payload)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
