use std::io::Cursor;

use actix::ResponseFuture;
use actix_web::{http::Method, pred, HttpRequest, HttpResponse, Path, State};
use bytes::BytesMut;
use failure::{Error, Fail};
use futures::{Future, IntoFuture, Stream};
use sentry::Hub;
use sentry_actix::ActixWebHubExt;
use tokio::codec::{BytesCodec, FramedRead};

use crate::actors::objects::{FetchObject, ObjectFileBytes, ObjectPurpose};
use crate::app::{ServiceApp, ServiceState};
use crate::sentry::SentryFutureExt;
use crate::types::Scope;
use crate::utils::paths::parse_symstore_path;

#[derive(Fail, Debug, Clone, Copy)]
pub enum ProxyErrorKind {
    #[fail(display = "failed to write object")]
    Io,

    #[fail(display = "failed to download object")]
    Fetching,
}

symbolic::common::derive_failure!(
    ProxyError,
    ProxyErrorKind,
    doc = "Errors happening while proxying to a symstore"
);

fn proxy_symstore_request(
    state: State<ServiceState>,
    request: HttpRequest<ServiceState>,
    path: Path<(String,)>,
) -> ResponseFuture<HttpResponse, Error> {
    let hub = Hub::from_request(&request);

    Hub::run(hub, || {
        let is_head = request.method() == Method::HEAD;

        if !state.config.symstore_proxy {
            return Box::new(Ok(HttpResponse::NotFound().finish()).into_future());
        }

        let (filetypes, object_id) = match parse_symstore_path(&path.0) {
            Some(x) => x,
            None => return Box::new(Ok(HttpResponse::NotFound().finish()).into_future()),
        };
        log::debug!("Searching for {:?} ({:?})", object_id, filetypes);
        Box::new(
            state
                .objects
                .fetch(FetchObject {
                    filetypes,
                    identifier: object_id,
                    sources: state.config.sources.clone(),
                    scope: Scope::Global,
                    purpose: ObjectPurpose::Debug,
                })
                .map_err(|e| e.context(ProxyErrorKind::Fetching).into())
                .and_then(move |object_file| {
                    if !object_file.has_object() {
                        return Ok(HttpResponse::NotFound().finish());
                    }
                    let length = object_file.len();
                    let mut response = HttpResponse::Ok();
                    response
                        .content_length(length)
                        .header("content-type", "application/octet-stream");
                    if is_head {
                        Ok(response.finish())
                    } else {
                        let bytes = Cursor::new(ObjectFileBytes(object_file));
                        let async_bytes = FramedRead::new(bytes, BytesCodec::new())
                            .map(BytesMut::freeze)
                            .map_err(|_err| ProxyError::from(ProxyErrorKind::Io))
                            .map_err(Error::from);
                        Ok(response.streaming(async_bytes))
                    }
                })
                .sentry_hub_current(),
        )
    })
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/symbols/{path:.+}", |r| {
        r.route()
            .filter(pred::Any(pred::Get()).or(pred::Head()))
            .with(proxy_symstore_request);
    })
}
