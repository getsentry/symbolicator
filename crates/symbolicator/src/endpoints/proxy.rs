use std::io::Cursor;
use std::sync::Arc;

use anyhow::Context;
use axum::body::Body;
use axum::extract;
use axum::http::{Method, Request, Response, StatusCode};

use symbolic::common::ByteView;
use symbolicator_service::caching::{CacheContents, CacheError};
use symbolicator_sources::parse_symstore_path;

use crate::service::{FindObject, ObjectMetaHandle, ObjectPurpose, RequestService, Scope};

use super::ResponseError;

/// Builds an empty 404 response. Inlined into 4 different match arms otherwise.
fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .expect("404 builder is infallible")
}

/// Microsoft-style compressed-extension suffix -> uncompressed last char.
///
/// Symbolicator's path generator (`paths.rs:475-485`) already requests both forms on ingress;
/// this table reverses the mapping for inbound proxy requests.
const COMPRESSED_EXTENSIONS: &[(&str, char)] = &[("pd_", 'b'), ("dl_", 'l'), ("ex_", 'e')];

/// If `leaf` looks like `<name>.{pd_,dl_,ex_}` (case-insensitive), returns the uncompressed
/// form with the trailing `_` replaced by `b`/`l`/`e`. Casing of the replacement matches the
/// casing of the original extension (lowercase input -> lowercase replacement, otherwise
/// uppercase). Returns `None` for any other input.
fn uncompress_leaf(leaf: &str) -> Option<String> {
    let dot = leaf.rfind('.')?;
    let ext = &leaf[dot + 1..];
    let (_, replacement) = COMPRESSED_EXTENSIONS
        .iter()
        .find(|(suffix, _)| suffix.eq_ignore_ascii_case(ext))?;

    let mut out = leaf[..leaf.len() - 1].to_owned();
    if ext.chars().any(|c| c.is_ascii_uppercase()) {
        out.push(replacement.to_ascii_uppercase());
    } else {
        out.push(*replacement);
    }
    Some(out)
}

/// If `path` looks like a symstore path whose last filename component ends in `_`
/// (`.pd_` / `.dl_` / `.ex_`), returns the path rewritten to the uncompressed form along with
/// the uncompressed leaf filename. Returns `None` for any other path.
///
/// Both the leading and trailing filename components of a symstore path
/// (`<filename>/<id>/<filename>`) are rewritten, matching the convention enforced by
/// [`parse_symstore_path`].
fn rewrite_compressed_path(path: &str) -> Option<(String, String)> {
    let first_slash = path.find('/')?;
    let last_slash = path.rfind('/')?;
    let trailing_uncompressed = uncompress_leaf(&path[last_slash + 1..])?;
    let leading = &path[..first_slash];
    let leading_uncompressed = uncompress_leaf(leading).unwrap_or_else(|| leading.to_owned());

    let middle = &path[first_slash..=last_slash];
    let rewritten = format!("{leading_uncompressed}{middle}{trailing_uncompressed}");
    Some((rewritten, trailing_uncompressed))
}

/// Resolves a (possibly rewritten) symstore path to an [`ObjectMetaHandle`] for the
/// requested object.
async fn find_meta(
    service: &RequestService,
    path: &str,
) -> CacheContents<Arc<ObjectMetaHandle>> {
    let config = service.config();
    if !config.symstore_proxy {
        return Err(CacheError::NotFound);
    }

    tracing::trace!("Resolving symstore proxy path `{path:?}`");

    let (filetypes, object_id) = parse_symstore_path(path).ok_or(CacheError::NotFound)?;

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

    meta.handle
}

pub async fn proxy_symstore_request(
    extract::State(service): extract::State<RequestService>,
    extract::Path(path): extract::Path<String>,
    request: Request<Body>,
) -> Result<Response<Body>, ResponseError> {
    sentry::configure_scope(|scope| {
        scope.set_transaction(Some("GET /proxy"));
    });

    // Detect a Microsoft-style compressed request (`.pd_` / `.dl_` / `.ex_`). When the
    // feature is enabled, `inner_filename` is `Some(<uncompressed leaf>)` -- its presence
    // is the sole signal that we should emit a CAB envelope instead of raw bytes.
    let (resolve_path, inner_filename) =
        match (rewrite_compressed_path(&path), service.config().compressed_proxy) {
            (Some((rewritten, leaf)), true) => (rewritten, Some(leaf)),
            (Some(_), false) => {
                // Compressed-proxy disabled: 404 the underscore form so we don't accidentally
                // expose unrelated decompressed bytes under a misleading filename.
                return Ok(not_found());
            }
            (None, _) => (path, None),
        };

    let meta = match find_meta(&service, &resolve_path).await {
        Ok(meta) => meta,
        Err(CacheError::NotFound) => return Ok(not_found()),
        Err(e) => {
            return Err(e)
                .context("failed to locate object")
                .map_err(|e| e.into());
        }
    };

    if let Some(inner_filename) = inner_filename {
        let bytes = match service
            .fetch_compressed_object(meta, &inner_filename)
            .await
        {
            Ok(b) => b,
            Err(CacheError::NotFound) => return Ok(not_found()),
            Err(e) => {
                return Err(e)
                    .context("failed to materialize compressed object")
                    .map_err(|e| e.into());
            }
        };

        return Ok(build_response(
            bytes,
            "application/vnd.ms-cab-compressed",
            request.method(),
        )?);
    }

    let object_handle = match service.fetch_object(meta).await {
        Ok(handle) => handle,
        Err(CacheError::NotFound) => return Ok(not_found()),
        Err(e) => {
            return Err(e)
                .context("failed to download object")
                .map_err(|e| e.into());
        }
    };

    let data = object_handle.data().clone();
    Ok(build_response(
        data,
        "application/octet-stream",
        request.method(),
    )?)
}

fn build_response(
    bytes: ByteView<'static>,
    content_type: &'static str,
    method: &Method,
) -> Result<Response<Body>, axum::http::Error> {
    let response = Response::builder()
        .header("content-length", bytes.len())
        .header("content-type", content_type);

    if *method == Method::HEAD {
        return response.body(Body::empty());
    }

    let cursor = Cursor::new(bytes);
    response.body(Body::from_stream(tokio_util::io::ReaderStream::new(cursor)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reqwest::{Client, StatusCode};

    use crate::test;

    use super::rewrite_compressed_path;

    #[test]
    fn test_rewrite_compressed_path() {
        let cases = [
            (
                "integration.pd_/ABC123/integration.pd_",
                Some(("integration.pdb/ABC123/integration.pdb", "integration.pdb")),
            ),
            (
                "kernel32.dl_/DEADBEEF/kernel32.dl_",
                Some(("kernel32.dll/DEADBEEF/kernel32.dll", "kernel32.dll")),
            ),
            (
                "Cmd.Ex_/FOO/Cmd.Ex_",
                Some(("Cmd.ExE/FOO/Cmd.ExE", "Cmd.ExE")),
            ),
            ("integration.pdb/abc/integration.pdb", None),
            ("integration.bin/abc/integration.bin", None),
            ("integration.zz_/abc/integration.zz_", None),
        ];
        for (input, expected) in cases {
            let actual = rewrite_compressed_path(input);
            match (actual, expected) {
                (None, None) => {}
                (Some((path, leaf)), Some((expected_path, expected_leaf))) => {
                    assert_eq!(path, expected_path, "rewriting {input}");
                    assert_eq!(leaf, expected_leaf, "leaf for {input}");
                }
                (a, e) => panic!("mismatch for {input}: got {a:?}, expected {e:?}"),
            }
        }
    }

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

    /// `.pd_` is 404 when `compressed_proxy` is disabled, even if the underlying `.pdb` exists.
    #[tokio::test]
    async fn test_proxy_compressed_off_returns_404() {
        test::setup();

        let (_srv, source) = test::symbol_server();
        let server = test::server_with_config(|config| {
            config.symstore_proxy = true;
            config.compressed_proxy = false;
            config.sources = Arc::from([source]);
        });

        let response = Client::new()
            .get(server.url(
                "/proxy/integration.pd_/0C1033F78632492E91C6C314B72E1920ffffffff/integration.pd_",
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// `.pd_` is served as a freshly-synthesized CAB when `compressed_proxy` is enabled and
    /// the source only has the decompressed `.pdb`.
    #[tokio::test]
    async fn test_proxy_compressed_synthesized() {
        use std::io::{Cursor, Read};

        test::setup();

        let (_srv, source) = test::symbol_server();
        let server = test::server_with_config(|config| {
            config.symstore_proxy = true;
            config.compressed_proxy = true;
            config.sources = Arc::from([source]);
        });

        let response = Client::new()
            .get(server.url(
                "/proxy/integration.pd_/0C1033F78632492E91C6C314B72E1920ffffffff/integration.pd_",
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok()),
            Some("application/vnd.ms-cab-compressed"),
        );

        let body = response.bytes().await.unwrap();
        assert!(body.starts_with(b"MSCF"), "expected a CAB envelope");

        // The CAB should contain the original PDB under its uncompressed name.
        let mut cab = cab::Cabinet::new(Cursor::new(&body[..])).unwrap();
        let entries: Vec<String> = cab
            .folder_entries()
            .flat_map(|f| f.file_entries())
            .map(|f| f.name().to_owned())
            .collect();
        assert_eq!(entries, vec!["integration.pdb".to_owned()]);

        let mut extracted = Vec::new();
        cab.read_file("integration.pdb")
            .unwrap()
            .read_to_end(&mut extracted)
            .unwrap();
        let expected = test::read_fixture(
            "symbols/integration.pdb/0C1033F78632492E91C6C314B72E1920ffffffff/integration.pdb",
        );
        assert_eq!(extracted, expected);
    }

    /// HEAD on `.pd_` returns the right headers without a body.
    #[tokio::test]
    async fn test_proxy_compressed_head() {
        test::setup();

        let (_srv, source) = test::symbol_server();
        let server = test::server_with_config(|config| {
            config.symstore_proxy = true;
            config.compressed_proxy = true;
            config.sources = Arc::from([source]);
        });

        let response = Client::new()
            .head(server.url(
                "/proxy/integration.pd_/0C1033F78632492E91C6C314B72E1920ffffffff/integration.pd_",
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok()),
            Some("application/vnd.ms-cab-compressed"),
        );
        let len: usize = response
            .headers()
            .get("content-length")
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(len > 0);
        let body = response.bytes().await.unwrap();
        assert!(body.is_empty(), "HEAD response must have empty body");
    }
}
