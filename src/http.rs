use actix_web::{
    client::{ClientRequest, ClientResponse, SendRequestError},
    FutureResponse, HttpMessage,
};

use futures::future::{Either, Future, IntoFuture};

pub fn follow_redirects(
    req: ClientRequest,
    max_redirects: usize,
) -> FutureResponse<ClientResponse, SendRequestError> {
    let headers_bak = req.headers().clone();

    Box::new(req.send().and_then(move |response| {
        if response.status().is_redirection() && max_redirects > 0 {
            if let Some(location) = response
                .headers()
                .get("Location")
                .and_then(|x| x.to_str().ok())
            {
                log::debug!("Following redirect: {:?}", location);
                let mut builder = ClientRequest::get(location);
                for (k, v) in headers_bak {
                    if let Some(k) = k {
                        if k != "Host" {
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
