use actix_multipart::{Field, Multipart};
use actix_web::web::Bytes;
use actix_web::{error, web, Error};
use futures::{future, Future, Stream};

use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::service::symbolication::SymbolicationActor;
use crate::service::Service;
use crate::types::{RequestId, Scope, SourceConfig, SymbolicationResponse};
use crate::utils::futures::ResultFuture;
use crate::utils::multipart::{read_multipart_file, read_multipart_sources};
use crate::utils::sentry::ToSentryScope;

#[derive(Debug, Default)]
struct MinidumpRequest {
    sources: Option<Vec<SourceConfig>>,
    minidump: Option<Bytes>,
}

fn handle_form_field(
    mut request: MinidumpRequest,
    field: Field,
) -> ResultFuture<MinidumpRequest, Error> {
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

fn process_minidump(
    symbolication: &SymbolicationActor,
    request: MinidumpRequest,
    scope: Scope,
) -> Result<RequestId, Error> {
    let minidump = request
        .minidump
        .ok_or_else(|| error::ErrorBadRequest("missing minidump"))?;

    let sources = request
        .sources
        .ok_or_else(|| error::ErrorBadRequest("missing sources"))?;

    Ok(symbolication.process_minidump(scope, minidump, sources))
}

fn post_minidump(
    service: web::Data<Service>,
    params: web::Query<SymbolicationRequestQueryParams>,
    multipart: Multipart,
) -> ResultFuture<web::Json<SymbolicationResponse>, Error> {
    log::trace!("Received minidump");

    let default_sources = service.config().default_sources();
    let symbolication = service.symbolication();

    let params = params.into_inner();
    params.configure_scope();

    let SymbolicationRequestQueryParams { scope, timeout } = params;
    let response = multipart
        .map_err(Error::from)
        .fold(MinidumpRequest::default(), move |request, item| {
            handle_form_field(request, item)
        })
        .and_then(clone!(symbolication, |mut request| {
            if request.sources.is_none() {
                request.sources = Some((*default_sources).clone());
            }

            process_minidump(&symbolication, request, scope)
        }))
        .and_then(move |request_id| {
            symbolication
                .get_response(request_id, timeout)
                .then(|result| match result {
                    Ok(Some(response)) => Ok(web::Json(response)),
                    Ok(None) => Err(error::ErrorInternalServerError(
                        "symbolication request did not start",
                    )),
                    Err(error) => Err(error::ErrorInternalServerError(error)),
                })
        });

    Box::new(response)
}

pub fn configure(config: &mut web::ServiceConfig) {
    config.route("/minidump", web::post().to(post_minidump));
}
