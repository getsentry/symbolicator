use std::io::Cursor;
use std::sync::Arc;

use actix_web::{http::Method, pred, App, HttpRequest, HttpResponse, Path, State};
use failure::{Error, ResultExt};
use futures01::Stream;
use tokio01::codec::{BytesCodec, FramedRead};

use crate::services::objects::{FindObject, ObjectHandle, ObjectPurpose};
use crate::services::Service;
use crate::types::Scope;
use crate::utils::paths::parse_symstore_path;

async fn load_object(state: &Service, path: &str) -> Result<Option<Arc<ObjectHandle>>, Error> {
    let config = state.config();
    if !config.symstore_proxy {
        return Ok(None);
    }

    let (filetypes, object_id) = match parse_symstore_path(path) {
        Some(tuple) => tuple,
        None => return Ok(None),
    };

    log::debug!("Searching for {:?} ({:?})", object_id, filetypes);

    let found_object = state
        .objects()
        .find(FindObject {
            filetypes,
            identifier: object_id,
            sources: config.default_sources(),
            scope: Scope::Global,
            purpose: ObjectPurpose::Debug,
        })
        .await
        .context("failed to download object")?;

    let object_meta = match found_object.meta {
        Some(meta) => meta,
        None => return Ok(None),
    };

    let object_handle = state
        .objects()
        .fetch(object_meta)
        .await
        .context("failed to download object")?;

    if object_handle.has_object() {
        Ok(Some(object_handle))
    } else {
        Ok(None)
    }
}

async fn proxy_symstore_request(
    state: State<Service>,
    request: HttpRequest<Service>,
    path: Path<(String,)>,
) -> Result<HttpResponse, Error> {
    let object_handle = match load_object(&state, &path.0).await? {
        Some(handle) => handle,
        None => return Ok(HttpResponse::NotFound().finish()),
    };

    let mut response = HttpResponse::Ok();
    response
        .content_length(object_handle.len() as u64)
        .header("content-type", "application/octet-stream");

    if request.method() == Method::HEAD {
        return Ok(response.finish());
    }

    let bytes = Cursor::new(object_handle.data());
    let async_bytes = FramedRead::new(bytes, BytesCodec::new()).map(|bytes| bytes.freeze());
    Ok(response.streaming(async_bytes))
}

pub fn configure(app: App<Service>) -> App<Service> {
    app.resource("/symbols/{path:.+}", |r| {
        r.route()
            .filter(pred::Any(pred::Get()).or(pred::Head()))
            .with_async(compat_handler!(proxy_symstore_request, s, r, p));
    })
}
