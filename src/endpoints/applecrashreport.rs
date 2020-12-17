use actix::ResponseFuture;
use actix_web::{
    dev::Payload, error, http::Method, multipart, Error, HttpMessage, HttpRequest, Json, Query,
    State,
};
use bytes::Bytes;
use futures01::{future, Future, Stream};
use sentry::{configure_scope, Hub};

use crate::actors::symbolication::SymbolicationActor;
use crate::app::{ServiceApp, ServiceState};
use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::sources::SourceConfig;
use crate::types::{RequestId, RequestOptions, Scope, SymbolicationResponse};
use crate::utils::futures::ThreadPool;
use crate::utils::multipart::{
    read_multipart_file, read_multipart_request_options, read_multipart_sources,
};
use crate::utils::sentry::{ActixWebHubExt, SentryFutureExt, WriteSentryScope};

#[derive(Debug, Default)]
struct AppleCrashReportRequest {
    sources: Option<Vec<SourceConfig>>,
    apple_crash_report: Option<Bytes>,
    options: RequestOptions,
}

fn handle_multipart_item(
    threadpool: ThreadPool,
    mut request: AppleCrashReportRequest,
    item: multipart::MultipartItem<Payload>,
) -> ResponseFuture<AppleCrashReportRequest, Error> {
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
        Some("apple_crash_report") => {
            let future = read_multipart_file(field).map(move |apple_crash_report| {
                request.apple_crash_report = Some(apple_crash_report);
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
    request: AppleCrashReportRequest,
    stream: multipart::Multipart<Payload>,
) -> ResponseFuture<AppleCrashReportRequest, Error> {
    let future = stream
        .map_err(Error::from)
        .fold(request, move |request, item| {
            handle_multipart_item(threadpool.clone(), request, item)
        });

    Box::new(future)
}

fn process_apple_crash_report(
    symbolication: &SymbolicationActor,
    request: AppleCrashReportRequest,
    scope: Scope,
) -> Result<RequestId, Error> {
    let report = request
        .apple_crash_report
        .ok_or_else(|| error::ErrorBadRequest("missing apple crash report"))?;

    let sources = request
        .sources
        .ok_or_else(|| error::ErrorBadRequest("missing sources"))?;

    Ok(symbolication.process_apple_crash_report(scope, report, sources, request.options))
}

fn handle_apple_crash_report_request(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    request: HttpRequest<ServiceState>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    let hub = Hub::from_request(&request);

    Hub::run(hub, || {
        let default_sources = state.config().default_sources();

        let params = params.into_inner();
        configure_scope(|scope| {
            params.write_sentry_scope(scope);
        });

        let request_future = handle_multipart_stream(
            state.io_pool(),
            AppleCrashReportRequest::default(),
            request.multipart(),
        );

        let SymbolicationRequestQueryParams { scope, timeout } = params;
        let symbolication = state.symbolication();

        let response_future = request_future
            .and_then(clone!(symbolication, |mut request| {
                if request.sources.is_none() {
                    request.sources = Some(default_sources.to_vec());
                }

                process_apple_crash_report(&symbolication, request, scope)
            }))
            .and_then(move |request_id| {
                symbolication
                    .get_response(request_id, timeout)
                    .then(|result| match result {
                        Ok(Some(response)) => Ok(Json(response)),
                        Ok(None) => Err(error::ErrorInternalServerError(
                            "symbolication request did not start",
                        )),
                        Err(error) => Err(error::ErrorInternalServerError(error)),
                    })
                    .map_err(Error::from)
            });

        Box::new(response_future.sentry_hub_current())
    })
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/applecrashreport", |r| {
        r.method(Method::POST)
            .with(handle_apple_crash_report_request);
    })
}
