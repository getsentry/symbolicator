use std::io::Cursor;

use actix_web::{http::Method, pred, HttpRequest, HttpResponse, Path, State};
use bytes::BytesMut;
use failure::{Error, Fail};
use futures::future::{FutureExt, TryFutureExt};
use futures01::{future::Either, Future, IntoFuture, Stream};
use sentry::Hub;
use tokio01::codec::{BytesCodec, FramedRead};

use crate::actors::objects::{FindObject, ObjectPurpose};
use crate::app::{ServiceApp, ServiceState};
use crate::types::Scope;
use crate::utils::futures::ResponseFuture;
use crate::utils::paths::parse_symstore_path;
use crate::utils::sentry::{ActixWebHubExt, SentryFutureExt};

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
            .boxed_local()
            .compat()
            .map_err(|e| e.context("failed to download object").into());

        let object_file_opt = object_meta_opt.and_then(move |object_meta_opt| {
            if let Some(object_meta) = object_meta_opt.meta {
                Either::A(
                    state
                        .objects()
                        .fetch(object_meta)
                        .compat()
                        .map_err(|e| e.context("failed to download object").into())
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
                        let bytes = Cursor::new(object_file.data());
                        let async_bytes = FramedRead::new(bytes, BytesCodec::new())
                            .map(BytesMut::freeze)
                            .map_err(|e| Error::from(e.context("failed to write object")));
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
