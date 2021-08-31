use std::io::{Seek, SeekFrom, Write};

use axum::extract;
use axum::http::StatusCode;
use axum::response::Json;

use crate::endpoints::map_max_requests_error;
use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::services::Service;
use crate::types::{RequestOptions, SymbolicationResponse};
use crate::utils::sentry::ConfigureScope;

use super::ResponseError;

pub async fn handle_minidump_request(
    extract::Extension(state): extract::Extension<Service>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    mut multipart: extract::Multipart,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let mut minidump = None;
    let mut sources = state.config().default_sources();
    let mut options = RequestOptions::default();

    while let Some(field) = multipart.next_field().await? {
        match field.name() {
            Some("upload_file_minidump") => {
                let mut minidump_file = tempfile::Builder::new();
                minidump_file.prefix("minidump").suffix(".dmp");
                let minidump_file = if let Some(tmp_dir) = state.config().cache_dir("tmp") {
                    minidump_file.tempfile_in(tmp_dir)
                } else {
                    minidump_file.tempfile()
                }?;
                let (mut file, temp_path) = minidump_file.into_parts();

                // TODO: stream this to file instead of reading to memory
                //let mut file = tokio::fs::File::from_std(file);
                //tokio::io::copy(&mut field.into_async_read(), &mut file).await?;
                let bytes = field.bytes().await?;
                file.write_all(bytes.as_ref())?;
                file.seek(SeekFrom::Start(0))?;

                minidump = Some(temp_path)
            }
            // TODO: limit these multipart fields to 1M
            Some("sources") => sources = serde_json::from_slice(&field.bytes().await?)?,
            Some("options") => options = serde_json::from_slice(&field.bytes().await?)?,
            _ => (), // Always ignore unknown fields.
        }
    }

    let minidump_file = minidump.ok_or_else(|| {
        ResponseError::WithStatus(StatusCode::BAD_REQUEST, "missing minidump".to_owned())
    })?;

    let symbolication = state.symbolication();
    let request_id = symbolication
        .process_minidump(params.scope, minidump_file, sources, options)
        .map_err(map_max_requests_error)?;

    match symbolication.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err(ResponseError::Anyhow(anyhow::anyhow!(
            "symbolication request did not start"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use reqwest::{multipart, Client, StatusCode};

    use crate::test;
    use crate::types::SymbolicationResponse;

    #[tokio::test]
    async fn test_basic() {
        test::setup();

        let service = test::default_service();
        let server = test::Server::with_service(service);

        let file_contents = test::read_fixture("windows.dmp");
        let file_part = multipart::Part::bytes(file_contents).file_name("windows.dmp");

        let form = multipart::Form::new()
            .part("upload_file_minidump", file_part)
            .text("sources", "[]");

        let response = Client::new()
            .post(server.url("/minidump"))
            .multipart(form)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.text().await.unwrap();
        let response = serde_json::from_str::<SymbolicationResponse>(&body).unwrap();
        insta::assert_yaml_snapshot!(response);
    }

    // This test is disabled because it locks up on CI. We have not found a way to reproduce this.
    #[allow(dead_code)]
    // #[tokio::test]
    async fn test_integration_microsoft() {
        // TODO: Move this test to E2E tests
        test::setup();

        let service = test::default_service();
        let server = test::Server::with_service(service);
        let source = test::microsoft_symsrv();

        let file_contents = test::read_fixture("windows.dmp");
        let file_part = multipart::Part::bytes(file_contents).file_name("windows.dmp");

        let form = multipart::Form::new()
            .part("upload_file_minidump", file_part)
            .text("sources", serde_json::to_string(&vec![source]).unwrap())
            .text("options", r#"{"dif_candidates":true}"#);

        let response = Client::new()
            .post(server.url("/minidump"))
            .multipart(form)
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

        let service = test::default_service();
        let server = test::Server::with_service(service);

        let file_contents = test::read_fixture("windows.dmp");
        let file_part = multipart::Part::bytes(file_contents).file_name("windows.dmp");

        let form = multipart::Form::new()
            .part("upload_file_minidump", file_part)
            .text("sources", "[]")
            .text("unknown", "value");

        let response = Client::new()
            .post(server.url("/minidump"))
            .multipart(form)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
