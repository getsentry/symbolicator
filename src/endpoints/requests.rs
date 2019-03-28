use actix::ResponseFuture;
use actix_web::{http::Method, HttpResponse, Path, Query, State};
use failure::{Error, Fail};
use futures::Future;

use crate::app::{ServiceApp, ServiceState};
use crate::types::{
    RequestMeta, RequestWithMeta, ResumedSymbolicationRequest, SymbolicationError,
    SymbolicationErrorKind,
};

fn resume_request(
    state: State<ServiceState>,
    request: Path<ResumedSymbolicationRequest>,
    meta: Query<RequestMeta>,
) -> ResponseFuture<HttpResponse, Error> {
    Box::new(
        state
            .symbolication
            .send(RequestWithMeta(request.into_inner(), meta.into_inner()))
            .map_err(|e| e.context(SymbolicationErrorKind::Mailbox))
            .map_err(SymbolicationError::from)
            .flatten()
            .map(|response_opt| match response_opt {
                Some(response) => HttpResponse::Ok().json(response),
                None => HttpResponse::NotFound().finish(),
            })
            .map_err(Error::from),
    )
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/requests/{request_id}", |r| {
        r.method(Method::GET).with(resume_request);
    })
}
