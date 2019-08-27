use actix_web::{error, web, Error};
use futures::Future;
use serde::Deserialize;

use crate::service::Service;
use crate::types::{RequestId, SymbolicationResponse};
use crate::utils::futures::ResultFuture;

/// Path parameters of the symbolication poll request.
#[derive(Debug, Deserialize)]
struct PollSymbolicationRequestPath {
    pub request_id: RequestId,
}

/// Query parameters of the symbolication poll request.
#[derive(Debug, Deserialize)]
struct PollSymbolicationRequestQueryParams {
    #[serde(default)]
    pub timeout: Option<u64>,
}

fn get_request(
    service: web::Data<Service>,
    path: web::Path<PollSymbolicationRequestPath>,
    query: web::Query<PollSymbolicationRequestQueryParams>,
) -> ResultFuture<web::Json<SymbolicationResponse>, Error> {
    log::trace!("Received poll request for id {}", path.request_id);

    let response = service
        .symbolication()
        .get_response(path.into_inner().request_id, query.into_inner().timeout)
        .map_err(error::ErrorInternalServerError)
        .and_then(|response_opt| match response_opt {
            Some(response) => Ok(web::Json(response)),
            None => Err(error::ErrorNotFound("Request does not exist")),
        });

    Box::new(response)
}

/// Adds the request poll endpoint to the app.
pub fn configure(config: &mut web::ServiceConfig) {
    config.route("/requests/{request_id}", web::get().to(get_request));
}
