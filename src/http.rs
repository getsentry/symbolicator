use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::tcp::ConnectFuture;
use tokio::net::TcpStream;
use tokio::timer::Delay;

use url::Url;

use actix::actors::resolver::{Connect, Resolve, Resolver, ResolverError};
use actix::{clock, Actor, Addr, Context, Handler, ResponseFuture};

use actix_web::client::{ClientRequest, ClientResponse, SendRequestError};
use actix_web::{FutureResponse, HttpMessage};

use futures::future::{Either, Future, IntoFuture};
use futures::{Async, Poll};

use ipnetwork::IpNetwork;

lazy_static::lazy_static! {
    static ref RESERVED_IP_BLOCKS: Vec<IpNetwork> = vec![
        "0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12",
        "192.0.0.0/29", "192.0.2.0/24", "192.88.99.0/24", "192.168.0.0/16", "198.18.0.0/15",
        "198.51.100.0/24", "224.0.0.0/4", "240.0.0.0/4", "255.255.255.255/32",
    ].into_iter().map(|x| x.parse().unwrap()).collect();
}

pub fn follow_redirects(
    uri: Url,
    make_request: Box<dyn Fn(Url) -> ClientRequest>,
    max_redirects: usize,
) -> FutureResponse<ClientResponse, SendRequestError> {
    let base = Url::parse(&uri.to_string());
    let req = make_request(uri);

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
                    target_uri,
                    Box::new(move |uri| {
                        let mut req = make_request(uri);
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
                    for network in &*RESERVED_IP_BLOCKS {
                        if network.contains(addr.ip()) {
                            metric!(counter("http.blacklisted_ip") += 1);
                            log::debug!(
                                "Blocked attempt to connect to reserved IP address: {}",
                                addr.ip()
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
