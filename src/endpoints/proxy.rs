use std::io::Cursor;

use actix::ResponseFuture;
use actix_web::{http::Method, pred, HttpRequest, HttpResponse, Path, State};
use bytes::BytesMut;
use failure::{Error, Fail};
use futures::{future, Future, Stream};
use itertools::Itertools;
use symbolic::common::{CodeId, DebugId};
use tokio::codec::{BytesCodec, FramedRead};

use crate::actors::objects::{FetchObject, ObjectFileBytes, ObjectPurpose};
use crate::app::{ServiceApp, ServiceState};
use crate::types::{FileType, ObjectId, Scope};

#[derive(Fail, Debug, Clone, Copy)]
pub enum ProxyErrorKind {
    #[fail(display = "failed to write object")]
    Io,

    #[fail(display = "failed sending message to objects actor")]
    Mailbox,

    #[fail(display = "failed to download object")]
    Fetching,
}

symbolic::common::derive_failure!(
    ProxyError,
    ProxyErrorKind,
    doc = "Errors happening while proxying to a symstore"
);

fn parse_symstore_path(path: &str) -> Option<(&'static [FileType], ObjectId)> {
    let (leading_fn, signature, trailing_fn) = path.splitn(3, '/').collect_tuple()?;

    let leading_fn_lower = leading_fn.to_lowercase();
    if !trailing_fn.eq_ignore_ascii_case("file.ptr")
        && !leading_fn_lower.eq_ignore_ascii_case(trailing_fn)
    {
        return None;
    }

    if leading_fn_lower.ends_with(".pdb") {
        Some((
            FileType::pe(),
            ObjectId {
                code_id: None,
                code_file: None,
                debug_id: Some(DebugId::from_breakpad(signature).ok()?),
                debug_file: Some(leading_fn.into()),
            },
        ))
    } else if leading_fn_lower.ends_with(".debug")
        || leading_fn_lower.ends_with(".dwarf")
        || signature.starts_with("mach-uuid-sym-")
    {
        None
    } else {
        Some((
            FileType::pe(),
            ObjectId {
                code_id: Some(CodeId::parse_hex(signature).ok()?),
                code_file: Some(leading_fn.into()),
                debug_id: None,
                debug_file: None,
            },
        ))
    }
}

fn proxy_symstore_request(
    state: State<ServiceState>,
    req: HttpRequest<ServiceState>,
    path: Path<(String,)>,
) -> ResponseFuture<HttpResponse, Error> {
    let is_head = req.method() == Method::HEAD;

    if !state.config.symstore_proxy {
        return Box::new(future::ok(HttpResponse::NotFound().finish()));
    }

    let (filetypes, object_id) = match parse_symstore_path(&path.0) {
        Some(x) => x,
        None => return Box::new(future::ok(HttpResponse::NotFound().finish())),
    };
    Box::new(
        state
            .objects
            .send(FetchObject {
                filetypes,
                identifier: object_id,
                sources: state.config.sources.clone(),
                scope: Scope::Proxy,
                purpose: ObjectPurpose::Debug,
            })
            .map_err(|e| e.context(ProxyErrorKind::Mailbox).into())
            .and_then(move |result| {
                let object_file = match result {
                    Ok(object_file) => object_file,
                    Err(_err) => {
                        return future::err(ProxyErrorKind::Fetching.into());
                    }
                };
                if !object_file.has_object() {
                    return future::ok(HttpResponse::NotFound().finish());
                }
                let length = object_file.len();
                let mut response = HttpResponse::Ok();
                response
                    .content_length(length)
                    .header("content-type", "application/octet-stream");
                if is_head {
                    future::ok(response.finish())
                } else {
                    let bytes = Cursor::new(ObjectFileBytes(object_file));
                    let async_bytes = FramedRead::new(bytes, BytesCodec::new())
                        .map(BytesMut::freeze)
                        .map_err(|_err| ProxyError::from(ProxyErrorKind::Io))
                        .map_err(Error::from);
                    future::ok(response.streaming(async_bytes))
                }
            }),
    )
}

pub fn register(app: ServiceApp) -> ServiceApp {
    app.resource("/symbols/{path:.+}", |r| {
        r.route()
            .filter(pred::Any(pred::Get()).or(pred::Head()))
            .with(proxy_symstore_request);
    })
}
