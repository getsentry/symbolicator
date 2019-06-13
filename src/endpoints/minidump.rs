use std::fs;
use std::sync::Arc;

use actix::ResponseFuture;
use actix_web::dev::Payload;
use actix_web::http::header::ContentDisposition;
use actix_web::http::Method;
use actix_web::{error, multipart, Error, HttpMessage, HttpRequest, Json, Query, State};
use futures::{Future, IntoFuture, Stream};
use sentry::{configure_scope, Hub};
use sentry_actix::ActixWebHubExt;
use tokio_threadpool::ThreadPool;

use crate::actors::symbolication::{GetSymbolicationStatus, ProcessMinidump};
use crate::app::{ServiceApp, ServiceState};
use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::sentry::{SentryFutureExt, WriteSentryScope};
use crate::types::{SourceConfig, SymbolicationResponse};
use crate::utils::multipart::{read_multipart_file, read_multipart_sources};

enum MultipartItem {
    MinidumpFile(fs::File),
    Sources(Vec<SourceConfig>),
}

fn handle_multipart_item(
    threadpool: Arc<ThreadPool>,
    item: multipart::MultipartItem<Payload>,
) -> Box<Stream<Item = MultipartItem, Error = Error>> {
    match item {
        multipart::MultipartItem::Field(field) => {
            match field
                .content_disposition()
                .as_ref()
                .and_then(ContentDisposition::get_name)
            {
                Some("sources") => Box::new(
                    read_multipart_sources(field)
                        .map(MultipartItem::Sources)
                        .into_stream(),
                ),
                Some("upload_file_minidump") => Box::new(
                    read_multipart_file(field, threadpool.clone())
                        .map(MultipartItem::MinidumpFile)
                        .into_stream(),
                ),
                _ => Box::new(
                    Err(error::ErrorBadRequest("unknown formdata field"))
                        .into_future()
                        .into_stream(),
                ),
            }
        }
        multipart::MultipartItem::Nested(mp) => Box::new(
            mp.map_err(Error::from)
                .map(clone!(threadpool, |x| handle_multipart_item(
                    threadpool.clone(),
                    x
                )))
                .flatten(),
        ),
    }
}

fn process_minidump(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    request: HttpRequest<ServiceState>,
) -> ResponseFuture<Json<SymbolicationResponse>, Error> {
    let hub = Hub::from_request(&request);

    Hub::run(hub, || {
        let threadpool = state.io_threadpool.clone();
        let symbolication = state.symbolication.clone();
        let default_sources = state.config.sources.clone();

        let params = params.into_inner();

        configure_scope(|scope| {
            params.write_sentry_scope(scope);
        });

        let SymbolicationRequestQueryParams { scope, timeout } = params;

        let internal_request = request
            .multipart()
            .map_err(Error::from)
            .map(clone!(threadpool, |x| handle_multipart_item(
                threadpool.clone(),
                x
            )))
            .flatten()
            .take(2)
            .fold((None, None), |(mut file_opt, mut sources_opt), field| {
                match (field, &file_opt, &sources_opt) {
                    (MultipartItem::MinidumpFile(file), None, _) => file_opt = Some(file),
                    (MultipartItem::Sources(sources), _, None) => sources_opt = Some(sources),
                    _ => {
                        return Err(error::ErrorBadRequest("missing formdata fields"));
                    }
                }

                Ok((file_opt, sources_opt))
            })
            .and_then(move |collect_state| match collect_state {
                (Some(file), requested_sources) => Ok(ProcessMinidump {
                    file,
                    sources: match requested_sources {
                        Some(sources) => Arc::new(sources),
                        None => default_sources,
                    },
                    scope,
                }),
                _ => Err(error::ErrorBadRequest("missing formdata fields")),
            });

        let request_id = internal_request.and_then(clone!(symbolication, |request| {
            symbolication
                .process_minidump(request)
                .map_err(error::ErrorInternalServerError)
        }));

        let response = request_id
            .and_then(clone!(symbolication, |request_id| {
                symbolication
                    .get_symbolication_status(GetSymbolicationStatus {
                        request_id,
                        timeout,
                    })
                    .map_err(error::ErrorInternalServerError)
            }))
            .map(|x| Json(x.expect("Race condition: Inserted request not found!")))
            .map_err(Error::from);

        Box::new(response.sentry_hub_current())
    })
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/minidump", |r| {
        r.method(Method::POST).with(process_minidump);
    })
}
