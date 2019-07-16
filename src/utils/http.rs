use std::sync::atomic::{AtomicBool, Ordering};

use actix_connect::{Address, Connect, ConnectError, Resolver, TcpConnector};
use actix_service::ServiceExt;
use actix_web::client::{
    Client, ClientRequest, ClientResponse, Connector, PayloadError, SendRequestError,
};
use actix_web::{error, http::header};
use bytes::Bytes;
use futures::{future, Future, Stream};
use ipnetwork::IpNetwork;
use url::Url;

// use actix::actors::resolver::{Connect, Resolve, Resolver, ResolverError};
// use actix::{clock, Actor, Addr, Context, Handler, ResponseFuture};

// use actix_web::client::{ClientRequest, ClientResponse, SendRequestError};
// use actix_web::{FutureResponse, HttpMessage};

// use futures::future::{Either, Future, IntoFuture};
// use futures::{Async, Poll};

lazy_static::lazy_static! {
    // https://en.wikipedia.org/wiki/Reserved_IP_addresses#IPv4
    static ref RESERVED_IP_BLOCKS: Vec<IpNetwork> = [
        "0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12",
        "192.0.0.0/29", "192.0.2.0/24", "192.88.99.0/24", "192.168.0.0/16", "198.18.0.0/15",
        "198.51.100.0/24", "224.0.0.0/4", "240.0.0.0/4", "255.255.255.255/32",
    ].iter().map(|x| x.parse().unwrap()).collect();
}

// TODO(ja): This is an ugly hack to avoid passing config through the entire system. Instead, we
// should make all services accessible to each other. This might require a change in the
// architecture of object actors, however (unless messages should continue to carry that piece of
// information).
static ALLOW_RESERVED_IPS: AtomicBool = AtomicBool::new(false);

std::thread_local! {
    /// TODO(ja): Doc
    static UNSAFE_CLIENT: Client = Client::default();

    /// TODO(ja): Doc
    static SAFE_CLIENT: Client = Client::build().connector(
        Connector::new().connector(
            Resolver::default()
                .and_then(filter_ip_addrs)
                .and_then(TcpConnector::new())
        ).finish()
    ).finish();
}

/// TODO(ja): Doc
fn filter_ip_addrs<T: Address>(mut connect: Connect<T>) -> Result<Connect<T>, ConnectError> {
    let mut addrs = connect
        .take_addrs()
        .filter(|addr| !RESERVED_IP_BLOCKS.iter().any(|net| net.contains(addr.ip())))
        .peekable();

    if addrs.peek().is_none() {
        return Err(ConnectError::NoRecords);
    }

    Ok(connect.set_addrs(addrs))
}

/// TODO(ja): Doc
pub fn allow_reserved_ips(allow: bool) {
    ALLOW_RESERVED_IPS.store(allow, Ordering::Relaxed);
}

/// TODO(ja): Doc
pub fn default_client() -> Client {
    if ALLOW_RESERVED_IPS.load(Ordering::Relaxed) {
        return unsafe_client();
    }

    SAFE_CLIENT.with(Client::clone)
}

/// TODO(ja): Doc
pub fn unsafe_client() -> Client {
    UNSAFE_CLIENT.with(Client::clone)
}

/// TODO(ja): Doc
pub fn follow_redirects<F>(
    initial_url: Url,
    max_redirects: usize,
    make_request: F,
) -> impl Future<
    Item = ClientResponse<impl Stream<Item = Bytes, Error = PayloadError>>,
    Error = SendRequestError,
>
where
    F: Fn(&str) -> ClientRequest,
{
    let state = (initial_url, max_redirects, make_request, true);
    future::loop_fn(state, |(url, redirects, make_request, trusted)| {
        let mut request = make_request(url.as_str());

        if !trusted {
            let headers = request.headers_mut();
            headers.remove(header::AUTHORIZATION);
            headers.remove(header::COOKIE);
        }

        request.send().and_then(move |response| {
            if response.status().is_redirection() && redirects > 0 {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|l| l.to_str().ok());

                if let Some(location) = location {
                    let redirect_url = url
                        .join(location)
                        .map_err(|e| error::ErrorInternalServerError("TODO(ja)"))?;

                    let is_same_origin = redirect_url.origin() == url.origin();

                    log::trace!("Following redirect: {:?}", &redirect_url);

                    return Ok(future::Loop::Continue((
                        redirect_url,
                        redirects - 1,
                        make_request,
                        trusted && is_same_origin,
                    )));
                }
            }

            Ok(future::Loop::Break(response))
        })
    })
}
