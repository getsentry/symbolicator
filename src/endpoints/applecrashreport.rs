use std::fs::File;

use actix_multipart::{Field, Multipart};
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
struct AppleCrashReportRequest {
    sources: Option<Vec<SourceConfig>>,
    apple_crash_report: Option<File>,
}

fn handle_form_field(
    mut request: AppleCrashReportRequest,
    field: Field,
) -> ResultFuture<AppleCrashReportRequest, Error> {
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
        _ => {
            let error = error::ErrorBadRequest("unknown formdata field");
            Box::new(future::err(error))
        }
    }
}

fn parse_apple_crash_report(
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

    Ok(symbolication.process_apple_crash_report(scope, report, sources))
}

fn post_applecrashreport(
    service: web::Data<Service>,
    params: web::Query<SymbolicationRequestQueryParams>,
    multipart: Multipart,
) -> ResultFuture<web::Json<SymbolicationResponse>, Error> {
    log::trace!("Received apple crash report");

    let default_sources = service.config().default_sources();
    let symbolication = service.symbolication();

    let params = params.into_inner();
    params.configure_scope();

    let SymbolicationRequestQueryParams { scope, timeout } = params;
    let response = multipart
        .map_err(Error::from)
        .fold(AppleCrashReportRequest::default(), move |request, item| {
            handle_form_field(request, item)
        })
        .and_then(clone!(symbolication, |mut request| {
            if request.sources.is_none() {
                request.sources = Some((*default_sources).clone());
            }

            parse_apple_crash_report(&symbolication, request, scope)
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
    config.route("/applecrashreport", web::post().to(post_applecrashreport));
}
