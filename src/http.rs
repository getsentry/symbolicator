use std::io;

use url::Url;

use actix_web::client::{ClientRequest, ClientResponse, SendRequestError};
use actix_web::{FutureResponse, HttpMessage};

use futures::future::{Either, Future, IntoFuture};

pub fn follow_redirects(
    make_request: Box<dyn Fn() -> ClientRequest>,
    max_redirects: usize,
) -> FutureResponse<ClientResponse, SendRequestError> {
    let req = make_request();
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

                log::trace!("Following redirect: {:?}", &target_uri);

                return Either::A(follow_redirects(
                    Box::new(move || {
                        let mut req = make_request();
                        if !same_host {
                            req.headers_mut().remove("authorization");
                            req.headers_mut().remove("cookie");
                        }
                        req
                    }),
                    max_redirects - 1,
                ));
            }
        }

        Either::B(Ok(response).into_future())
    }))
}
