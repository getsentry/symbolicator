use std::io;

use url::Url;

use actix_web::client::{ClientRequest, ClientResponse, SendRequestError};
use actix_web::{FutureResponse, HttpMessage};

use futures::future::{Either, Future, IntoFuture};

pub fn follow_redirects(
    req: ClientRequest,
    max_redirects: usize,
) -> FutureResponse<ClientResponse, SendRequestError> {
    let headers_bak = req.headers().clone();
    let base = Url::parse(&req.uri().to_string());

    Box::new(req.send().and_then(move |response| {
        if response.status().is_redirection() && max_redirects > 0 {
            if let Some(location) = response
                .headers()
                .get("location")
                .and_then(|x| x.to_str().ok())
            {
                let base = match base.as_ref() {
                    Ok(base) => base,
                    Err(err) => {
                        return Either::B(
                            Err(SendRequestError::Io(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("bad request uri: {}", err),
                            )))
                            .into_future(),
                        );
                    }
                };
                let target_uri = match base.join(location) {
                    Ok(uri) => uri,
                    Err(err) => {
                        return Either::B(
                            Err(SendRequestError::Io(io::Error::new(
                                io::ErrorKind::Other,
                                format!("bad redirect: {}", err),
                            )))
                            .into_future(),
                        );
                    }
                };
                let same_host = target_uri.origin() == base.origin();

                log::debug!("Following redirect: {:?}", &target_uri);
                let mut builder = ClientRequest::get(&target_uri);
                for (k, v) in headers_bak {
                    if let Some(k) = k {
                        if k == "host" || (!same_host && (k == "authorization" || k == "cookie")) {
                            log::debug!("Dropping header: {:?}: {:?}", k, v);
                        } else {
                            log::debug!("Preserving header: {:?}: {:?}", k, v);
                            builder.header(k, v);
                        }
                    }
                }
                return Either::A(follow_redirects(
                    builder.finish().unwrap(),
                    max_redirects - 1,
                ));
            }
        }

        Either::B(Ok(response).into_future())
    }))
}
