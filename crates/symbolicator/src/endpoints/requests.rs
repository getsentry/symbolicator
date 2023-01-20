use axum::extract;
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::service::{RequestId, RequestService, SymbolicationResponse};

/// Query parameters of the symbolication poll request.
#[derive(Deserialize)]
pub struct PollSymbolicationRequestQueryParams {
    #[serde(default)]
    pub timeout: Option<u64>,
}

pub async fn poll_request(
    extract::State(service): extract::State<RequestService>,
    extract::Path(request_id): extract::Path<RequestId>,
    extract::Query(query): extract::Query<PollSymbolicationRequestQueryParams>,
) -> Result<Json<SymbolicationResponse>, StatusCode> {
    sentry::configure_scope(|scope| {
        scope.set_transaction(Some("GET /requests"));
    });

    let response_opt = service.get_response(request_id, query.timeout).await;

    match response_opt {
        Some(response) => Ok(Json(response)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::endpoints::symbolicate::SymbolicationRequestBody;
    use crate::test;

    use reqwest::Client;
    use symbolicator_sources::{DirectoryLayoutType, FileType};

    #[track_caller]
    fn ensure_request_id(response: SymbolicationResponse) -> RequestId {
        match response {
            SymbolicationResponse::Pending { request_id, .. } => request_id,
            res => panic!("expected a pending response, got: {res:#?}"),
        }
    }

    /// Tests that we respond according to the `timeout` query parameter with a `pending` status.
    /// We can query that request ID multiple times until it finally resolves.
    #[tokio::test]
    async fn test_timeouts() {
        test::setup();

        let client = Client::new();
        let server = test::server_with_default_service();
        let hitcounter = test::Server::new();

        let payload = r##"{
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
            "options": {"dif_candidates":true},
            "sources": []
        }"##;

        let mut payload: SymbolicationRequestBody = serde_json::from_str(payload).unwrap();
        let config = test::source_config(DirectoryLayoutType::Symstore, vec![FileType::Pdb]);
        let source = hitcounter.source_with_config("timeout", "/delay/2500ms/msdl/", config);
        payload.sources = Some(vec![source]);

        let response = client
            .post(server.url("/symbolicate?timeout=1"))
            .json(&payload)
            .send()
            .await
            .unwrap();

        let request_id = ensure_request_id(response.json().await.unwrap());

        let response = client
            .get(server.url(&format!("/requests/{request_id}?timeout=1")))
            .send()
            .await
            .unwrap();

        let request_id = ensure_request_id(response.json().await.unwrap());

        let response = client
            .get(server.url(&format!("/requests/{request_id}")))
            .send()
            .await
            .unwrap();

        let response: SymbolicationResponse = response.json().await.unwrap();
        test::assert_snapshot!(response);
    }
}
