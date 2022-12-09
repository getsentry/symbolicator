use std::io::Cursor;
use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::extract;
use axum::http::{Method, Request, Response, StatusCode};

use symbolicator_service::cache::{CacheEntry, CacheError};
use symbolicator_sources::parse_symstore_path;

use crate::service::{FindObject, ObjectHandle, ObjectPurpose, RequestService, Scope};

use super::ResponseError;

async fn load_object(service: RequestService, path: String) -> CacheEntry<Arc<ObjectHandle>> {
    let config = service.config();
    if !config.symstore_proxy {
        return Err(CacheError::NotFound);
    }

    let (filetypes, object_id) = parse_symstore_path(&path).ok_or(CacheError::NotFound)?;

    tracing::debug!("Searching for {:?} ({:?})", object_id, filetypes);

    let found_object = service
        .find_object(FindObject {
            filetypes,
            identifier: object_id,
            sources: config.default_sources(),
            scope: Scope::Global,
            purpose: ObjectPurpose::Debug,
        })
        .await?;

    let object_meta = found_object.meta.ok_or(CacheError::NotFound)?;

    service.fetch_object(object_meta).await
}

pub async fn proxy_symstore_request(
    extract::Extension(service): extract::Extension<RequestService>,
    extract::Path(path): extract::Path<String>,
    request: Request<Body>,
) -> Result<Response<Body>, ResponseError> {
    sentry::configure_scope(|scope| {
        scope.set_transaction(Some("GET /proxy"));
    });

    let object_handle = match load_object(service, path).await {
        Ok(handle) => handle,
        Err(CacheError::NotFound) => {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())?)
        }
        Err(e) => {
            return Err(e)
                .context("failed to download object")
                .map_err(|e| e.into())
        }
    };

    let data = object_handle.data().clone();
    let response = Response::builder()
        .header("content-length", data.len())
        .header("content-type", "application/octet-stream");

    if *request.method() == Method::HEAD {
        return Ok(response.body(Body::empty())?);
    }

    let bytes = Cursor::new(data);
    Ok(response.body(Body::wrap_stream(tokio_util::io::ReaderStream::new(bytes)))?)
}
