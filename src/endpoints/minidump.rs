use actix_web::{error, multipart, App, Error, HttpMessage, HttpRequest, Json, Query, State};
use futures::{compat::Stream01CompatExt, StreamExt};

use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::services::Service;
use crate::types::{RequestOptions, SymbolicationResponse};
use crate::utils::multipart::{
    read_multipart_file, read_multipart_request_options, read_multipart_sources,
};
use crate::utils::sentry::ConfigureScope;

async fn handle_minidump_request(
    state: State<Service>,
    params: Query<SymbolicationRequestQueryParams>,
    request: HttpRequest<Service>,
) -> Result<Json<SymbolicationResponse>, Error> {
    sentry::start_session();

    let params = params.into_inner();
    params.configure_scope();

    let mut minidump = None;
    let mut sources = state.config().default_sources();
    let mut options = RequestOptions::default();

    let mut stream = request.multipart().compat();
    while let Some(item) = stream.next().await {
        let field = match item? {
            multipart::MultipartItem::Field(field) => field,
            _ => return Err(error::ErrorBadRequest("unsupported nested formdata")),
        };

        let content_disposition = field.content_disposition();
        match content_disposition.as_ref().and_then(|d| d.get_name()) {
            Some("upload_file_minidump") => minidump = Some(read_multipart_file(field).await?),
            Some("sources") => sources = read_multipart_sources(field).await?.into(),
            Some("options") => options = read_multipart_request_options(field).await?,
            _ => (), // Always ignore unknown fields.
        }
    }

    let minidump = minidump.ok_or_else(|| error::ErrorBadRequest("missing minidump"))?;

    let symbolication = state.symbolication();
    let request_id = symbolication.process_minidump(params.scope, minidump, sources, options);

    match symbolication.get_response(request_id, params.timeout).await {
        Some(response) => Ok(Json(response)),
        None => Err(error::ErrorInternalServerError(
            "symbolication request did not start",
        )),
    }
}

pub fn configure(app: App<Service>) -> App<Service> {
    app.resource("/minidump", |r| {
        let handler = compat_handler!(handle_minidump_request, s, p, r);
        r.post().with_async(handler);
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use actix_web::test::TestServer;
    use reqwest::{multipart, Client, StatusCode};

    use crate::config::Config;
    use crate::services::Service;
    use crate::test;
    use crate::types::SymbolicationResponse;

    #[tokio::test]
    async fn test_basic() {
        test::setup();

        let service = Service::create(Config::default()).unwrap();
        let server = TestServer::with_factory(move || crate::server::create_app(service.clone()));

        let file_contents = fs::read("tests/fixtures/windows.dmp").unwrap();
        let file_part = multipart::Part::bytes(file_contents).file_name("windows.dmp");

        let form = multipart::Form::new()
            .part("upload_file_minidump", file_part)
            .text("sources", "[]");

        let response = Client::new()
            .post(&server.url("/minidump"))
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

        let service = Service::create(Config::default()).unwrap();
        let server = TestServer::with_factory(move || crate::server::create_app(service.clone()));
        let source = test::microsoft_symsrv();

        let file_contents = fs::read("tests/fixtures/windows.dmp").unwrap();
        let file_part = multipart::Part::bytes(file_contents).file_name("windows.dmp");

        let form = multipart::Form::new()
            .part("upload_file_minidump", file_part)
            .text("sources", serde_json::to_string(&vec![source]).unwrap())
            .text("options", r#"{"dif_candidates":true}"#);

        let response = Client::new()
            .post(&server.url("/minidump"))
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

        let service = Service::create(Config::default()).unwrap();
        let server = TestServer::with_factory(move || crate::server::create_app(service.clone()));

        let file_contents = fs::read("tests/fixtures/windows.dmp").unwrap();
        let file_part = multipart::Part::bytes(file_contents).file_name("windows.dmp");

        let form = multipart::Form::new()
            .part("upload_file_minidump", file_part)
            .text("sources", "[]")
            .text("unknown", "value");

        let response = Client::new()
            .post(&server.url("/minidump"))
            .multipart(form)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
