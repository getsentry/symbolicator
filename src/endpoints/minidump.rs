use actix::ResponseFuture;
use actix_web::{
    dev::Payload, error, http::Method, multipart, Error, HttpMessage, HttpRequest, Json, Query,
    State,
};
use bytes::Bytes;
use futures::{future, Future, Stream};
use sentry::{configure_scope, Hub};
use sentry_actix::ActixWebHubExt;

use crate::actors::symbolication::{GetSymbolicationStatus, SymbolicationActor};
use crate::app::{ServiceApp, ServiceState};
use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::types::{RequestId, Scope, SourceConfig, SymbolicationResponse};
use crate::utils::futures::ThreadPool;
use crate::utils::multipart::{read_multipart_file, read_multipart_sources};
use crate::utils::sentry::{SentryFutureExt, WriteSentryScope};

#[derive(Debug, Default)]
struct MinidumpRequest {
    sources: Option<Vec<SourceConfig>>,
    minidump: Option<Bytes>,
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
        _ => {
            let error = error::ErrorBadRequest("unknown formdata field");
            Box::new(future::err(error))
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

    symbolication
        .process_minidump(scope, minidump, sources)
        .map_err(error::ErrorInternalServerError)
}

fn handle_minidump_request(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    request: HttpRequest<ServiceState>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    let hub = Hub::from_request(&request);

    Hub::run(hub, || {
        let default_sources = state.config.default_sources();

        let params = params.into_inner();
        configure_scope(|scope| {
            params.write_sentry_scope(scope);
        });

        let io_pool = state.io_threadpool.clone();
        let request_future =
            handle_multipart_stream(io_pool, MinidumpRequest::default(), request.multipart());

        let SymbolicationRequestQueryParams { scope, timeout } = params;
        let symbolication = state.symbolication.clone();

        let response_future = request_future
            .and_then(clone!(symbolication, |mut request| {
                if request.sources.is_none() {
                    request.sources = Some((*default_sources).clone());
                }

                process_minidump(&symbolication, request, scope)
            }))
            .and_then(move |request_id| {
                symbolication
                    .get_symbolication_status(GetSymbolicationStatus {
                        request_id,
                        timeout,
                    })
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
    app.resource("/minidump", |r| {
        r.method(Method::POST).with(handle_minidump_request);
    })
}
