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

lazy_static::lazy_static! {
    // https://en.wikipedia.org/wiki/Reserved_IP_addresses#IPv4
    static ref RESERVED_IP_NETS: Vec<IpNetwork> = [
        "0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12",
        "192.0.0.0/29", "192.0.2.0/24", "192.88.99.0/24", "192.168.0.0/16", "198.18.0.0/15",
        "198.51.100.0/24", "224.0.0.0/4", "240.0.0.0/4", "255.255.255.255/32",
    ].iter().map(|x| x.parse().unwrap()).collect();
}

/// Global configuration whether connections to reserved IPs are allowed.
///
/// This is an ugly hack to avoid passing config through the entire system. Instead, we should make
/// all services accessible to each other. This might require a change in the architecture of object
/// actors, however (unless messages should continue to carry that piece of information).
static ALLOW_RESERVED_IPS: AtomicBool = AtomicBool::new(false);

// TODO: AwcConnector share

std::thread_local! {
    /// An HTTP client that allows connections to internal networks.
    static UNSAFE_CLIENT: Client = Client::default();

    /// An HTTP client that blocks connections to internal networks.
    static SAFE_CLIENT: Client = Client::build().connector(
        Connector::new().connector(
            Resolver::default()
                .and_then(filter_ip_addrs)
                .and_then(TcpConnector::new())
        ).finish()
    ).finish();
}

/// A service that filters connect messages to internal IPs.
fn filter_ip_addrs<T: Address>(mut connect: Connect<T>) -> Result<Connect<T>, ConnectError> {
    let mut addrs = connect
        .take_addrs()
        .filter(|addr| !RESERVED_IP_NETS.iter().any(|net| net.contains(addr.ip())))
        .peekable();

    // The resolver returns a connect error if no records have been found. Even though the TCP
    // connector will also perform a similar check, this is technically more correct behavior.
    if addrs.peek().is_none() {
        return Err(ConnectError::NoRecords);
    }

    Ok(connect.set_addrs(addrs))
}

/// Configures whether connections to internal networks are allowed.
///
/// By default, this is disabled. When setting this to true, the default client will not block
/// connections to localhost or any local IP ranges. This corresponds to the
/// `connect_to_reserved_ips` configuration option.
pub fn allow_reserved_ips(allow: bool) {
    ALLOW_RESERVED_IPS.store(allow, Ordering::Relaxed);
}

/// Returns the default HTTP client.
///
/// The client contains a shared connection per thread. Between threads, clients are not shared.
///
/// By default, this client blocks connections to hosts in the internal network. This can bee
/// changed via `allow_reserved_ips`.
pub fn default_client() -> Client {
    if ALLOW_RESERVED_IPS.load(Ordering::Relaxed) {
        unsafe_client()
    } else {
        SAFE_CLIENT.with(Client::clone)
    }
}

/// Returns an HTTP client that always allows connections to internal hosts.
///
/// The client contains a shared connection per thread. Between threads, clients are not shared.
pub fn unsafe_client() -> Client {
    UNSAFE_CLIENT.with(Client::clone)
}

/// Follows redirects and returns the final response.
///
/// When a redirect changes the host name, authorization and cookie headers are removed from the
/// request. The redirect loop is bounded by `max_redirects`. If this number is exceeded, the final
/// response is returned, regardless of whether it is a redirect.
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
                        .map_err(error::ErrorInternalServerError)?;

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
