use std::fs;
use std::io::Write;
use std::sync::Arc;
use tempfile;

use actix::ResponseFuture;
use actix_web::{
    dev::Payload, error, http::Method, multipart, Error, HttpMessage, HttpRequest, Json, Query,
    State,
};
use bytes::BytesMut;
use failure::Fail;
use futures::{
    future::{self, IntoFuture},
    Future, Stream,
};
use tokio_threadpool::ThreadPool;

use crate::actors::symbolication::{GetSymbolicationStatus, ProcessMinidump};
use crate::app::{ServiceApp, ServiceState};
use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::types::{
    SourceConfig, SymbolicationError, SymbolicationErrorKind, SymbolicationResponse,
};

enum MultipartItem {
    MinidumpFile(fs::File),
    Sources(Vec<SourceConfig>),
}

fn read_sources_json(
    field: multipart::Field<Payload>,
) -> impl Future<Item = MultipartItem, Error = Error> {
    field
        .map_err(Error::from)
        .fold(BytesMut::with_capacity(512), move |mut body, chunk| {
            if (body.len() + chunk.len()) > 2048 {
                Err(Error::from(error::JsonPayloadError::Overflow))
            } else {
                body.extend_from_slice(&chunk);
                Ok(body)
            }
        })
        .and_then(|body| Ok(serde_json::from_slice(&body)?))
        .map(MultipartItem::Sources)
}

fn read_minidump(
    threadpool: Arc<ThreadPool>,
    field: multipart::Field<Payload>,
) -> impl Future<Item = MultipartItem, Error = Error> {
    tempfile::tempfile()
        .into_future()
        .map_err(Error::from)
        .and_then(move |file| {
            field
                .map_err(Error::from)
                .fold(file, move |mut file, chunk| {
                    threadpool
                        .spawn_handle(future::lazy(move || file.write_all(&chunk).map(|_| file)))
                        .map_err(Error::from)
                })
        })
        .map(MultipartItem::MinidumpFile)
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
                .and_then(|x| x.get_name())
            {
                Some("sources") => Box::new(read_sources_json(field).into_stream()),
                Some("upload_file_minidump") => {
                    Box::new(read_minidump(threadpool.clone(), field).into_stream())
                }
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
    let threadpool = state.io_threadpool.clone();
    let symbolication = state.symbolication.clone();

    let SymbolicationRequestQueryParams { scope, timeout } = params.into_inner();

    let request = request
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
                    return Err(Error::from(error::ErrorBadRequest(
                        "missing formdata fields",
                    )));
                }
            }

            Ok((file_opt, sources_opt))
        })
        .and_then(|collect_state| match collect_state {
            (Some(file), Some(sources)) => Ok(ProcessMinidump {
                file,
                sources,
                scope,
            }),
            _ => Err(Error::from(error::ErrorBadRequest(
                "missing formdata fields",
            ))),
        });

    let request_id = request.and_then(clone!(symbolication, |request| symbolication
        .send(request)
        .map_err(|e| e.context(SymbolicationErrorKind::Mailbox))
        .map_err(SymbolicationError::from)
        .map_err(error::ErrorInternalServerError)
        .and_then(|result| Ok(result.map_err(error::ErrorInternalServerError)?))));

    let response = request_id
        .and_then(clone!(symbolication, |request_id| symbolication
            .send(GetSymbolicationStatus {
                request_id,
                timeout
            })
            .map_err(|e| e.context(SymbolicationErrorKind::Mailbox))
            .map_err(SymbolicationError::from)
            .map_err(error::ErrorInternalServerError)
            .and_then(|result| Ok(
                result.map_err(error::ErrorInternalServerError)?
            ))))
        .and_then(|response_opt| {
            response_opt
                .ok_or_else(|| error::ErrorInternalServerError(SymbolicationErrorKind::Mailbox))
        })
        .map(Json)
        .map_err(Error::from);

    Box::new(response)
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/minidump", |r| {
        r.method(Method::POST).with(process_minidump);
    })
}
