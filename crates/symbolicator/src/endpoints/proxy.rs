use std::io::Cursor;
use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::extract;
use axum::http::{Method, Request, Response, StatusCode};

use symbolicator_service::caching::{CacheContents, CacheError};
use symbolicator_sources::parse_symstore_path;

use crate::service::{FindObject, ObjectHandle, ObjectPurpose, RequestService, Scope};

use super::ResponseError;

async fn load_object(service: RequestService, path: String) -> CacheContents<Arc<ObjectHandle>> {
    let config = service.config();
    if !config.symstore_proxy {
        return Err(CacheError::NotFound);
    }

    tracing::trace!("Resolving symstore proxy path `{path:?}`");

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
        .await;

    let Some(meta) = found_object.meta else {
        return Err(CacheError::NotFound);
    };

    service.fetch_object(meta.handle?).await
}

pub async fn proxy_symstore_request(
    extract::State(service): extract::State<RequestService>,
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
    Ok(response.body(Body::from_stream(tokio_util::io::ReaderStream::new(bytes)))?)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reqwest::{Client, StatusCode};

    use crate::test;

    #[tokio::test]
    async fn test_proxy_works() {
        test::setup();

        let (_srv, source) = test::symbol_server();
        let server = test::server_with_config(|config| {
            config.symstore_proxy = true;
            config.sources = Arc::from([source]);
        });

        let response = Client::new()
            .get(server.url(
                "/proxy/integration.pdb/0C1033F78632492E91C6C314B72E1920ffffffff/integration.pdb",
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.bytes().await.unwrap();
        let file_contents = test::read_fixture(
            "symbols/integration.pdb/0C1033F78632492E91C6C314B72E1920ffffffff/integration.pdb",
        );
        assert_eq!(bytes, file_contents);
    }
}
