use std::net::IpAddr;

use ipnetwork::Ipv4Network;

use crate::config::Config;

lazy_static::lazy_static! {
    static ref RESERVED_IP_BLOCKS: Vec<Ipv4Network> = vec![
        // https://en.wikipedia.org/wiki/Reserved_IP_addresses#IPv4
        "0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12",
        "192.0.0.0/29", "192.0.2.0/24", "192.88.99.0/24", "192.168.0.0/16", "198.18.0.0/15",
        "198.51.100.0/24", "224.0.0.0/4", "240.0.0.0/4", "255.255.255.255/32",
    ].into_iter().map(|x| x.parse().unwrap()).collect();
}

fn is_external_ip(ip: std::net::IpAddr) -> bool {
    let addr = match ip {
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
}

pub fn create_client(config: &Config, trusted: bool) -> reqwest::Client {
    let mut builder = reqwest::ClientBuilder::new().gzip(true).trust_dns(true);

    if !(trusted || config.connect_to_reserved_ips) {
        builder = builder.ip_filter(is_external_ip);
    }

    builder.build().unwrap()
}

#[cfg(test)]
mod tests {
    use warp::Filter;

    use super::*;

    use crate::test;

    #[tokio::test]
    async fn test_untrusted_client() {
        test::setup();

        let server = test::Server::new(warp::get().and(warp::path::end()).map(|| "OK"));

        let config = Config {
            connect_to_reserved_ips: false,
            ..Config::default()
        };

        let result = create_client(&config, false) // untrusted
            .get(server.url("/"))
            .send()
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_untrusted_client_loopback() {
        test::setup();

        let server = test::Server::new(warp::get().and(warp::path::end()).map(|| "OK"));

        let config = Config {
            connect_to_reserved_ips: false,
            ..Config::default()
        };

        let result = create_client(&config, false) // untrusted
            .get(&format!("http://127.0.0.1:{}/", server.addr().port()))
            .send()
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_untrusted_client_allowed() {
        test::setup();

        let server = test::Server::new(warp::get().and(warp::path::end()).map(|| "OK"));

        let config = Config {
            connect_to_reserved_ips: true,
            ..Config::default()
        };

        let response = create_client(&config, false) // untrusted
            .get(server.url("/"))
            .send()
            .await
            .unwrap();

        let text = response.text().await.unwrap();
        assert_eq!(text, "OK");
    }

    #[tokio::test]
    async fn test_trusted() {
        test::setup();

        let server = test::Server::new(warp::get().and(warp::path::end()).map(|| "OK"));

        let config = Config {
            connect_to_reserved_ips: false,
            ..Config::default()
        };

        let response = create_client(&config, true) // trusted
            .get(server.url("/"))
            .send()
            .await
            .unwrap();

        let text = response.text().await.unwrap();
        assert_eq!(text, "OK");
    }
}
