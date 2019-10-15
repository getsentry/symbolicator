use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use actix::actors::resolver::{Connect, Resolve, Resolver, ResolverError};
use actix::{clock, Actor, Addr, Context, Handler, ResponseFuture, System};
use actix_web::client::{ClientConnector, ClientRequest, ClientResponse, SendRequestError};
use actix_web::{http::header, HttpMessage};
use futures::{future, future::Either, Async, Future, IntoFuture, Poll};
use ipnetwork::Ipv4Network;
use tokio::net::{tcp::ConnectFuture, TcpStream};
use tokio::timer::Delay;
use url::Url;

lazy_static::lazy_static! {
    static ref RESERVED_IP_BLOCKS: Vec<Ipv4Network> = vec![
        // https://en.wikipedia.org/wiki/Reserved_IP_addresses#IPv4
        "0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12",
        "192.0.0.0/29", "192.0.2.0/24", "192.88.99.0/24", "192.168.0.0/16", "198.18.0.0/15",
        "198.51.100.0/24", "224.0.0.0/4", "240.0.0.0/4", "255.255.255.255/32",
    ].into_iter().map(|x| x.parse().unwrap()).collect();
}

pub fn follow_redirects<F>(
    initial_url: Url,
    max_redirects: usize,
    make_request: F,
) -> ResponseFuture<ClientResponse, SendRequestError>
where
    F: Fn(&str) -> ClientRequest + Send + 'static,
{
    let state = (initial_url, max_redirects, true);
    Box::new(future::loop_fn(state, move |(url, redirects, trusted)| {
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
                    let redirect_url = url.join(location).map_err(|e| {
                        let message = format!("bad request uri: {}", e);
                        SendRequestError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
                    })?;

                    log::trace!("Following redirect: {:?}", &redirect_url);
                    let is_same_origin = redirect_url.origin() == url.origin();

                    return Ok(future::Loop::Continue((
                        redirect_url,
                        redirects - 1,
                        trusted && is_same_origin,
                    )));
                }
            }

            Ok(future::Loop::Break(response))
        })
    }))
}

/// A Resolver-like actor for the actix-web http client that refuses to resolve to reserved IP addresses.
pub struct SafeResolver(Addr<Resolver>);

impl Default for SafeResolver {
    fn default() -> Self {
        SafeResolver(Resolver::default().start())
    }
}

impl Actor for SafeResolver {
    type Context = Context<Self>;
}

impl Handler<Connect> for SafeResolver {
    type Result = ResponseFuture<TcpStream, ResolverError>;

    fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
        println!("RECEIVING CONNECT: {:?}", msg);

        let Connect {
            name,
            port,
            timeout,
        } = msg;

        let fut = self
            .0
            .send(Resolve { name, port })
            .map_err(|mailbox_e| ResolverError::Resolver(mailbox_e.to_string()))
            .flatten()
            .and_then(move |mut addrs| {
                addrs.retain(|addr| {
                    let addr = match addr.ip() {
                        IpAddr::V4(x) => x,
                        IpAddr::V6(_) => {
                            // We don't know what is an internal service in IPv6 and what is not. Just
                            // bail out. This effectively means that we don't support IPv6.
                            return false;
                        }
                    };

                    for network in &*RESERVED_IP_BLOCKS {
                        if network.contains(addr) {
                            metric!(counter("http.blocked_ip") += 1);
                            log::debug!(
                                "Blocked attempt to connect to reserved IP address: {}",
                                addr
                            );
                            return false;
                        }
                    }

                    true
                });

                if addrs.is_empty() {
                    Either::A(
                        Err(ResolverError::InvalidInput(
                            "Blocked attempt to connect to reserved IP addresses",
                        ))
                        .into_future(),
                    )
                } else {
                    Either::B(TcpConnector::with_timeout(addrs, timeout))
                }
            });

        Box::new(fut)
    }
}

/// Tcp stream connector, copied from `actix::actors::resolver::TcpConnector`. The only change is
/// that this one implements `Future` while the original only implements ActorFuture.
struct TcpConnector {
    addrs: VecDeque<SocketAddr>,
    timeout: Delay,
    stream: Option<ConnectFuture>,
}

impl TcpConnector {
    pub fn with_timeout(addrs: VecDeque<SocketAddr>, timeout: Duration) -> TcpConnector {
        TcpConnector {
            addrs,
            stream: None,
            timeout: Delay::new(clock::now() + timeout),
        }
    }
}

impl Future for TcpConnector {
    type Item = TcpStream;
    type Error = ResolverError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // timeout
        if let Ok(Async::Ready(_)) = self.timeout.poll() {
            return Err(ResolverError::Timeout);
        }

        // connect
        loop {
            if let Some(new) = self.stream.as_mut() {
                match new.poll() {
                    Ok(Async::Ready(sock)) => return Ok(Async::Ready(sock)),
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Err(err) => {
                        if self.addrs.is_empty() {
                            return Err(ResolverError::IoError(err));
                        }
                    }
                }
            }

            // try to connect
            let addr = self.addrs.pop_front().unwrap();
            self.stream = Some(TcpStream::connect(&addr));
        }
    }
}

pub fn start_safe_connector() {
    let connector = ClientConnector::default()
        .resolver(SafeResolver::default().start().recipient())
        .start();

    System::with_current(|sys| sys.registry().set(connector));
}

#[cfg(test)]
mod tests {
    use super::*;

    use actix_web::Error;

    use crate::test;

    #[test]
    fn test_local_connection() -> Result<(), Error> {
        test::setup();
        start_safe_connector();

        let server = test::TestServer::new(|app| {
            app.resource("/", |resource| resource.f(|_| "OK"));
        });

        let response = test::block_fn(|| server.get().finish().unwrap().send());
        assert!(response.is_err());

        Ok(())
    }
}
