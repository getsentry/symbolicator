use anyhow::Context;
use axum::body::Body;
use axum::extract;
use axum::http::{Method, Request, Response, StatusCode};

use std::io::Cursor;
use std::sync::Arc;

use crate::services::objects::{FindObject, ObjectHandle, ObjectPurpose};
use crate::services::Service;
use crate::types::Scope;
use crate::utils::paths::parse_symstore_path;

use super::ResponseError;

async fn load_object(state: Service, path: String) -> anyhow::Result<Option<Arc<ObjectHandle>>> {
    let config = state.config();
    if !config.symstore_proxy {
        return Ok(None);
    }

    let (filetypes, object_id) = match parse_symstore_path(&path) {
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

pub async fn proxy_symstore_request(
    extract::Extension(state): extract::Extension<Service>,
    extract::Path(path): extract::Path<String>,
    request: Request<Body>,
) -> Result<Response<Body>, ResponseError> {
    let object_handle = match load_object(state, path).await? {
        Some(handle) => handle,
        None => {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())?)
        }
    };

    let response = Response::builder()
        .header("content-length", object_handle.len())
        .header("content-type", "application/octet-stream");

    if *request.method() == Method::HEAD {
        return Ok(response.body(Body::empty())?);
    }

    let bytes = Cursor::new(object_handle.data());
    Ok(response.body(Body::wrap_stream(tokio_util::io::ReaderStream::new(bytes)))?)
}
