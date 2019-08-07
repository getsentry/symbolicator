use std::io::Cursor;

use actix_web::{error, guard, http, web, Error, HttpRequest, HttpResponse};
use bytes::BytesMut;
use futures::{future, Future, Stream};
use serde::Deserialize;
use tokio::codec::{BytesCodec, FramedRead};

use crate::service::objects::{FindObject, ObjectPurpose};
use crate::service::Service;
use crate::types::Scope;
use crate::utils::paths::parse_symstore_path;

/// Path parameters of the symstore proxy request.
#[derive(Debug, Deserialize)]
struct ProxyPath {
    pub path: String,
}

fn get_symstore_proxy(
    service: web::Data<Service>,
    path: web::Path<ProxyPath>,
    request: HttpRequest,
) -> Box<dyn Future<Item = HttpResponse, Error = Error>> {
    let is_head = request.method() == http::Method::HEAD;

    if !service.config().symstore_proxy {
        log::trace!("Ignoring proxy request (disabled in config)");
        return Box::new(future::ok(HttpResponse::NotFound().finish()));
    }

    log::trace!("Received proxy {} request", request.method());

    let (filetypes, object_id) = match parse_symstore_path(&path.path) {
        Some((filetypes, object_id)) => (filetypes, object_id),
        None => return Box::new(future::ok(HttpResponse::NotFound().finish())),
    };

    log::debug!("Searching for {:?} ({:?})", object_id, filetypes);

    let objects = service.objects();
    let response = objects
        .find(FindObject {
            filetypes,
            identifier: object_id,
            sources: service.config().default_sources(),
            scope: Scope::Global,
            purpose: ObjectPurpose::Debug,
        })
        .and_then(move |meta_opt| match meta_opt {
            Some(meta) => future::Either::A(objects.fetch(meta).map(Some)),
            None => future::Either::B(future::ok(None)),
        })
        .map_err(error::ErrorInternalServerError)
        .and_then(|object_opt| {
            if let Some(object) = object_opt {
                if object.has_object() {
                    return Ok(object);
                }
            }
            Err(error::ErrorNotFound("File does not exist"))
        })
        .and_then(move |object_file| {
            let mut response = HttpResponse::Ok();
            response
                .content_length(object_file.len() as u64)
                .set(http::header::ContentType::octet_stream());

            if is_head {
                Ok(response.finish())
            } else {
                let bytes = Cursor::new(object_file.data());
                let async_bytes = FramedRead::new(bytes, BytesCodec::new())
                    .map(BytesMut::freeze)
                    .map_err(Error::from);
                Ok(response.streaming(async_bytes))
            }
        });

    Box::new(response)
}

pub fn configure(config: &mut web::ServiceConfig) {
    config.route(
        "/symbols/{path:.+}",
        web::get()
            .guard(guard::Any(guard::Get()).or(guard::Head()))
            .to(get_symstore_proxy),
    );
}
