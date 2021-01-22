use actix_web::{
    dev::Payload, error, http::Method, multipart, Error, HttpMessage, HttpRequest, Json, Query,
    State,
};
use bytes::Bytes;
use futures::{FutureExt, TryFutureExt};
use futures01::{future, Future, Stream};
use sentry::{configure_scope, Hub};

use crate::actors::symbolication::SymbolicationActor;
use crate::app::{ServiceApp, ServiceState};
use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::sources::SourceConfig;
use crate::types::{RequestId, RequestOptions, Scope, SymbolicationResponse};
use crate::utils::futures::{ResponseFuture, ThreadPool};
use crate::utils::multipart::{
    read_multipart_file, read_multipart_request_options, read_multipart_sources,
};
use crate::utils::sentry::{ActixWebHubExt, SentryFutureExt, WriteSentryScope};

#[derive(Debug, Default)]
struct MinidumpRequest {
    sources: Option<Vec<SourceConfig>>,
    minidump: Option<Bytes>,
    options: RequestOptions,
}

fn handle_multipart_item(
    threadpool: ThreadPool,
    mut request: MinidumpRequest,
    item: multipart::MultipartItem<Payload>,
) -> ResponseFuture<MinidumpRequest, Error> {
    let field = match item {
        multipart::MultipartItem::Field(field) => field,
        multipart::MultipartItem::Nested(nested) => {
            return handle_multipart_stream(threadpool, request, nested);
        }
    };

    match field
        .content_disposition()
        .as_ref()
        .and_then(|d| d.get_name())
    {
        Some("sources") => {
            let future = read_multipart_sources(field).map(move |sources| {
                request.sources = Some(sources);
                request
            });
            Box::new(future)
        }
        Some("upload_file_minidump") => {
            let future = read_multipart_file(field).map(move |minidump| {
                request.minidump = Some(minidump);
                request
            });
            Box::new(future)
        }
        Some("options") => {
            let future = read_multipart_request_options(field).map(move |options| {
                request.options = options;
                request
            });
            Box::new(future)
        }
        _ => {
            // Always ignore unknown fields.
            Box::new(future::ok(request))
        }
    }
}

fn handle_multipart_stream(
    threadpool: ThreadPool,
    request: MinidumpRequest,
    stream: multipart::Multipart<Payload>,
) -> ResponseFuture<MinidumpRequest, Error> {
    let future = stream
        .map_err(Error::from)
        .fold(request, move |request, item| {
            handle_multipart_item(threadpool.clone(), request, item)
        });

    Box::new(future)
}

fn process_minidump(
    symbolication: &SymbolicationActor,
    request: MinidumpRequest,
    scope: Scope,
) -> Result<RequestId, Error> {
    let minidump = request
        .minidump
        .ok_or_else(|| error::ErrorBadRequest("missing minidump"))?;

    sentry::configure_scope(|scope| {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&minidump);
        scope.set_extra("minidump_crc32", hasher.finalize().into());
        scope.set_extra("minidump_len", minidump.len().into());
    });

    let sources = request
        .sources
        .ok_or_else(|| error::ErrorBadRequest("missing sources"))?;

    Ok(symbolication.process_minidump(scope, minidump, sources, request.options))
}

fn handle_minidump_request(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    request: HttpRequest<ServiceState>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    let hub = Hub::from_request(&request);
    hub.start_session();

    Hub::run(hub, || {
        let default_sources = state.config().default_sources();

        let params = params.into_inner();
        configure_scope(|scope| {
            params.write_sentry_scope(scope);
        });

        let request_future = handle_multipart_stream(
            state.io_pool(),
            MinidumpRequest::default(),
            request.multipart(),
        );

        let SymbolicationRequestQueryParams { scope, timeout } = params;
        let symbolication = state.symbolication();

        let response_future = request_future
            .and_then(clone!(symbolication, |mut request| {
                if request.sources.is_none() {
                    request.sources = Some(default_sources.to_vec());
                }

                process_minidump(&symbolication, request, scope)
            }))
            .and_then(move |request_id| {
                symbolication
                    .get_response(request_id, timeout)
                    .never_error()
                    .boxed_local()
                    .compat()
                    .then(|result| match result {
                        Ok(Some(response)) => Ok(Json(response)),
                        Ok(None) => Err(error::ErrorInternalServerError(
                            "symbolication request did not start",
                        )),
                        Err(never) => match never {},
                    })
                    .map_err(Error::from)
            });

        Box::new(response_future.sentry_hub_current())
    })
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/minidump", |r| {
        r.method(Method::POST).with(handle_minidump_request);
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use actix_web::test::TestServer;
    use reqwest::{multipart, Client, StatusCode};

    use crate::app::ServiceState;
    use crate::config::Config;
    use crate::test;
    use crate::types::SymbolicationResponse;

    #[tokio::test]
    async fn test_basic() {
        test::setup();

        let service = ServiceState::create(Config::default()).unwrap();
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

        let service = ServiceState::create(Config::default()).unwrap();
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

        let service = ServiceState::create(Config::default()).unwrap();
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
