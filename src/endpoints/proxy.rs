use std::io::Cursor;

use actix::ResponseFuture;
use actix_web::{http::Method, pred, HttpRequest, HttpResponse, Path, State};
use bytes::BytesMut;
use failure::{Error, Fail};
use futures01::{future::Either, Future, IntoFuture, Stream};
use sentry::Hub;
use sentry_actix::ActixWebHubExt;
use tokio::codec::{BytesCodec, FramedRead};

use crate::actors::objects::{FindObject, ObjectFileBytes, ObjectPurpose};
use crate::app::{ServiceApp, ServiceState};
use crate::types::Scope;
use crate::utils::paths::parse_symstore_path;
use crate::utils::sentry::SentryFutureExt;

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

    let is_head = request.method() == Method::HEAD;
    let config = state.config();

    if !config.symstore_proxy {
        return Box::new(Ok(HttpResponse::NotFound().finish()).into_future());
    }

    let (filetypes, object_id) = match parse_symstore_path(&path.0) {
        Some(x) => x,
        None => return Box::new(Ok(HttpResponse::NotFound().finish()).into_future()),
    };

    Hub::run(hub, || {
        log::debug!("Searching for {:?} ({:?})", object_id, filetypes);

        let object_meta_opt = state
            .objects()
            .find(FindObject {
                filetypes,
                identifier: object_id,
                sources: config.default_sources(),
                scope: Scope::Global,
                purpose: ObjectPurpose::Debug,
            })
            .map_err(|e| e.context(ProxyErrorKind::Fetching).into());

        let object_file_opt = object_meta_opt.and_then(move |object_meta_opt| {
            if let Some(object_meta) = object_meta_opt {
                Either::A(
                    state
                        .objects()
                        .fetch(object_meta)
                        .map_err(|e| e.context(ProxyErrorKind::Fetching).into())
                        .map(Some),
                )
            } else {
                Either::B(Ok(None).into_future())
            }
        });

        Box::new(
            object_file_opt
                .and_then(move |object_file_opt| {
                    let object_file = if object_file_opt
                        .as_ref()
                        .map(|x| x.has_object())
                        .unwrap_or(false)
                    {
                        object_file_opt.unwrap()
                    } else {
                        return Ok(HttpResponse::NotFound().finish());
                    };

                    let length = object_file.len();
                    let mut response = HttpResponse::Ok();
                    response
                        .content_length(length as u64)
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

pub fn configure(app: ServiceApp) -> ServiceApp {
    app.resource("/symbols/{path:.+}", |r| {
        r.route()
            .filter(pred::Any(pred::Get()).or(pred::Head()))
            .with(proxy_symstore_request);
    })
}
