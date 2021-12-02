use axum::extract;
use axum::http::StatusCode;
use axum::response::Json;
use tokio::fs::File;

use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::services::Service;
use crate::types::{RequestOptions, SymbolicationResponse};
use crate::utils::sentry::ConfigureScope;

use super::multipart::{read_multipart_data, stream_multipart_file};
use super::ResponseError;

pub async fn handle_apple_crash_report_request(
    extract::Extension(state): extract::Extension<Service>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    mut multipart: extract::Multipart,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let mut report = None;
    let mut sources = state.config().default_sources();
    let mut options = RequestOptions::default();

    while let Some(field) = multipart.next_field().await? {
        match field.name() {
            Some("apple_crash_report") => {
                let mut report_file = File::from_std(tempfile::tempfile()?);
                stream_multipart_file(field, &mut report_file).await?;
                report = Some(report_file.into_std().await)
            }
            Some("sources") => {
                let data = read_multipart_data(field, 1024 * 1024).await?; // 1Mb
                sources = serde_json::from_slice(&data)?;
            }
            Some("options") => {
                let data = read_multipart_data(field, 1024 * 1024).await?; // 1Mb
                options = serde_json::from_slice(&data)?
            }
            _ => (), // Always ignore unknown fields.
        }
    }

    let report = report.ok_or((StatusCode::BAD_REQUEST, "missing apple crash report"))?;

    let symbolication = state.symbolication();
    let request_id =
        symbolication.process_apple_crash_report(params.scope, report, sources, options)?;

    match symbolication.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
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

        let service = test::default_service().await;
        let server = test::Server::with_service(service);

        let file_contents = test::read_fixture("apple_crash_report.txt");
        let file_part = multipart::Part::bytes(file_contents).file_name("apple_crash_report.txt");

        let form = multipart::Form::new()
            .part("apple_crash_report", file_part)
            .text("sources", "[]");

        let response = Client::new()
            .post(server.url("/applecrashreport"))
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

        let service = test::default_service().await;
        let server = test::Server::with_service(service);

        let file_contents = test::read_fixture("apple_crash_report.txt");
        let file_part = multipart::Part::bytes(file_contents).file_name("apple_crash_report.txt");

        let form = multipart::Form::new()
            .part("apple_crash_report", file_part)
            .text("sources", "[]")
            .text("unknown", "value");

        let response = Client::new()
            .post(server.url("/applecrashreport"))
            .multipart(form)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
