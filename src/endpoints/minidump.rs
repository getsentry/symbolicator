use actix_web::{error, multipart, Error, HttpMessage, HttpRequest, Json, Query, State};
use futures::{compat::Stream01CompatExt, StreamExt};
use sentry::configure_scope;

use crate::app::{ServiceApp, ServiceState};
use crate::endpoints::symbolicate::SymbolicationRequestQueryParams;
use crate::types::SymbolicationResponse;
use crate::utils::multipart::{read_multipart_file, read_multipart_sources};
use crate::utils::sentry::WriteSentryScope;

async fn handle_minidump_request(
    state: State<ServiceState>,
    params: Query<SymbolicationRequestQueryParams>,
    request: HttpRequest<ServiceState>,
) -> Result<Json<SymbolicationResponse>, Error> {
    let params = params.into_inner();
    configure_scope(|scope| {
        params.write_sentry_scope(scope);
    });

    let mut minidump = None;
    let mut sources = None;

    let mut stream = request.multipart().compat();
    while let Some(item) = stream.next().await {
        let field = match item? {
            multipart::MultipartItem::Field(field) => field,
            _ => return Err(error::ErrorBadRequest("unsupported nested formdata")),
        };

        let content_disposition = field.content_disposition();
        match content_disposition.as_ref().and_then(|d| d.get_name()) {
            Some("sources") => sources = Some(read_multipart_sources(field).await?),
            Some("upload_file_minidump") => minidump = Some(read_multipart_file(field).await?),
            _ => return Err(error::ErrorBadRequest("unknown formdata field")),
        }
    }

    let minidump = match minidump {
        Some(minidump) => minidump,
        None => return Err(error::ErrorBadRequest("missing minidump")),
    };

    let sources = match sources {
        Some(sources) => sources,
        None => (*state.config().default_sources()).clone(),
    };

    let request_id = state
        .symbolication()
        .process_minidump(params.scope, minidump, sources)
        .map_err(error::ErrorInternalServerError)?;

    let result = state
        .symbolication()
        .get_response(request_id, params.timeout)
        .await;

    match result {
        Ok(Some(response)) => Ok(Json(response)),
        Ok(None) => Err(error::ErrorInternalServerError(
            "symbolication request did not start",
        )),
        Err(error) => Err(error::ErrorInternalServerError(error)),
    }
}

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/minidump", |r| {
        let handler = compat_handler!(handle_minidump_request, s, p, r);
        r.post().with_async(handler);
    })
}
