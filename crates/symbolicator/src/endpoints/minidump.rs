use axum::extract;
use axum::http::StatusCode;
use axum::response::Json;
use symbolic::common::ByteView;
use symbolicator_native::interface::{AttachmentFile, ProcessMinidump};
use tokio::fs::File;

use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::metric;
use crate::service::{RequestOptions, RequestService, SymbolicationResponse};
use crate::utils::sentry::ConfigureScope;

use super::ResponseError;
use super::multipart::{read_multipart_data, stream_multipart_file};

pub async fn handle_minidump_request(
    extract::State(service): extract::State<RequestService>,
    extract::Query(params): extract::Query<SymbolicationRequestQueryParams>,
    mut multipart: extract::Multipart,
) -> Result<Json<SymbolicationResponse>, ResponseError> {
    sentry::start_session();

    params.configure_scope();

    let mut minidump = None;
    let mut sources = service.config().default_sources();
    let mut scraping = Default::default();
    let mut options = RequestOptions::default();
    let mut platform = None;
    let mut rewrite_first_module = Default::default();

    while let Some(field) = multipart.next_field().await? {
        match field.name() {
            Some("upload_file_minidump") => {
                let mut minidump_file = File::from_std(tempfile::tempfile()?);
                stream_multipart_file(field, &mut minidump_file).await?;
                minidump = Some(minidump_file.into_std().await)
            }
            Some("sources") => {
                let data = read_multipart_data(field, 1024 * 1024).await?; // 1Mb
                sources = serde_json::from_slice(&data)?;
            }
            Some("scraping") => {
                let data = read_multipart_data(field, 1024 * 1024).await?; // 1Mb
                scraping = serde_json::from_slice(&data)?;
            }
            Some("options") => {
                let data = read_multipart_data(field, 1024 * 1024).await?; // 1Mb
                options = serde_json::from_slice(&data)?;
            }
            Some("platform") => {
                let data = read_multipart_data(field, 1024 * 1024).await?; // 1Mb
                platform = serde_json::from_slice(&data)?
            }
            Some("rewrite_first_module") => {
                let data = read_multipart_data(field, 1024 * 1024).await?; // 1Mb
                rewrite_first_module = serde_json::from_slice(&data)?
            }
            _ => (), // Always ignore unknown fields.
        }
    }

    let minidump_file = minidump.ok_or((StatusCode::BAD_REQUEST, "missing minidump"))?;

    // check if the minidump starts with multipart form data and discard it if so
    let minidump =
        ByteView::map_file_ref(&minidump_file).unwrap_or_else(|_| ByteView::from_slice(b""));
    if minidump.starts_with(b"--") {
        metric!(counter("symbolication.minidump.multipart_form_data") += 1);
        return Err((
            StatusCode::BAD_REQUEST,
            "minidump contains multipart form data",
        )
            .into());
    }

    let request_id = service.process_minidump(
        ProcessMinidump {
            platform,
            scope: params.scope,
            minidump_file: AttachmentFile::Local(minidump_file),
            sources,
            scraping,
            rewrite_first_module,
        },
        options,
    )?;

    match service.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err("symbolication request did not start".into()),
    }
}

#[cfg(test)]
mod tests {
    use reqwest::{Client, StatusCode, multipart};

    use crate::service::SymbolicationResponse;
    use crate::test;

    #[tokio::test]
    async fn test_basic() {
        test::setup();

        let server = test::server_with_default_service();

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

        let response: SymbolicationResponse = response.json().await.unwrap();
        test::assert_snapshot!(response);
    }

    #[tokio::test]
    async fn test_integration_microsoft() {
        test::setup();

        let server = test::server_with_default_service();
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

        let response: SymbolicationResponse = response.json().await.unwrap();
        test::assert_snapshot!(response);
    }

    #[tokio::test]
    async fn test_unknown_field() {
        test::setup();

        let server = test::server_with_default_service();

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

    #[tokio::test]
    async fn test_body_limit() {
        test::setup();

        let server = test::server_with_config(|config| {
            config.crash_file_body_max_bytes = 10 * 1024;
        });

        let len = 9 * 1024;
        let mut buf = vec![b'.'; len];
        buf[0..4].copy_from_slice(b"MDMP");

        let file_part =
            multipart::Part::stream_with_length(buf, len as u64).file_name("minidump.dmp");

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

        // well, the minidump is obviously invalid :-)
        let body = response.text().await.unwrap();
        assert_eq!(
            &body,
            "{\"status\":\"failed\",\"message\":\"Minidump version mismatch\"}"
        );

        let len = 11 * 1024;
        let mut buf = vec![b'.'; len];
        buf[0..4].copy_from_slice(b"MDMP");

        let file_part =
            multipart::Part::stream_with_length(buf, len as u64).file_name("minidump.dmp");

        let form = multipart::Form::new()
            .part("upload_file_minidump", file_part)
            .text("sources", "[]");

        let response = Client::new()
            .post(server.url("/minidump"))
            .multipart(form)
            .send()
            .await;

        // FIXME(swatinem): it is a bit unclear which error we get exactly, and from which internal
        // parts. This can give us an internal server error or a connection reset depending on OS
        // right now. Ideally, it should give us a `PAYLOAD_TOO_LARGE`, but that might require some
        // more work inside of `axum`, see <https://github.com/tokio-rs/axum/issues/1623>.
        assert!(response.is_err() || !response.unwrap().status().is_success());
    }
}
