use std::fmt::Write;
use std::net::IpAddr;
use std::time::Duration;

use ipnetwork::Ipv4Network;
use once_cell::sync::Lazy;
use reqwest::{redirect, StatusCode, Url};

use crate::config::Config;

static RESERVED_IP_BLOCKS: Lazy<Vec<Ipv4Network>> = Lazy::new(|| {
    [
        // https://en.wikipedia.org/wiki/Reserved_IP_addresses#IPv4
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/29",
        "192.0.2.0/24",
        "192.88.99.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "224.0.0.0/4",
        "240.0.0.0/4",
        "255.255.255.255/32",
    ]
    .into_iter()
    .map(|x| x.parse().unwrap())
    .collect()
});

fn is_external_ip(ip: std::net::IpAddr) -> bool {
    let IpAddr::V4(addr) = ip else {
        // We don't know what is an internal service in IPv6 and what is not. Just
        // bail out. This effectively means that we don't support IPv6.
        return false;
    };

    for network in &*RESERVED_IP_BLOCKS {
        if network.contains(addr) {
            metric!(counter("http.blocked_ip") += 1);
            tracing::debug!(
                "Blocked attempt to connect to reserved IP address: {}",
                addr
            );
            return false;
        }
    }

    true
}

/// Various timeouts for all the Downloaders
#[derive(Copy, Clone, Debug)]
pub struct DownloadTimeouts {
    /// The timeout for establishing a connection.
    pub connect: Duration,
    /// The timeout for receiving the first headers.
    pub head: Duration,
    /// An adaptive timeout per 1GB of content.
    pub streaming: Duration,
    /// Global timeout for one download.
    pub max_download: Duration,
}

impl DownloadTimeouts {
    pub fn from_config(config: &Config) -> Self {
        Self {
            connect: config.connect_timeout,
            head: config.head_timeout,
            streaming: config.streaming_timeout,
            max_download: config.max_download_timeout,
        }
    }
}

impl Default for DownloadTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_millis(500),
            head: Duration::from_secs(5),
            streaming: Duration::from_secs(250),
            max_download: Duration::from_secs(315),
        }
    }
}

/// Creates a [`reqwest::Client`] with the provided options.
///
/// * `timeouts` controls connection and download timeouts.
/// * `connect_to_reserved_ips` determines whether the client is allowed to
///   connect to reserved IPs (as defined in `RESERVED_IP_BLOCKS`).
/// * `accept_invalid_certs` determines whether the client accepts invalid
///   SSL certificates.
/// * Uses a custom redirect policy that limits redirects from certain hosts
///   to avoid fetching login pages.
pub fn create_client(
    timeouts: &DownloadTimeouts,
    connect_to_reserved_ips: bool,
    accept_invalid_certs: bool,
) -> reqwest::Client {
    let mut builder = reqwest::ClientBuilder::new()
        .gzip(true)
        .hickory_dns(true)
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.max_download)
        .pool_idle_timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(accept_invalid_certs)
        .redirect(redirect::Policy::custom(|attempt: redirect::Attempt| {
            // The default redirect policy allows to follow up to 10 redirects. This is problematic
            // when symbolicator tries to fetch native source files from a web source, as a redirect
            // might land us on a login page, which is then used for source context.
            // To avoid this, symbolicator's redirect policy is to not follow temporary redirects
            // on hosts that are known to redirect to login pages.

            if attempt.status() == StatusCode::FOUND {
                let is_from_azure = attempt
                    .previous()
                    .last()
                    .and_then(|url| url.host_str())
                    .map_or(false, |host| host == "dev.azure.com");

                if is_from_azure {
                    return attempt.stop();
                }
            }
            redirect::Policy::default().redirect(attempt)
        }));

    if !connect_to_reserved_ips {
        builder = builder.ip_filter(is_external_ip);
    }

    builder.build().unwrap()
}

#[derive(Debug, Clone)]
struct AllowedOriginPattern {
    scheme: String,
    domain: String,
    path: String,
}

impl AllowedOriginPattern {
    fn parse(value: &str) -> Option<Self> {
        let (scheme, rest) = value.split_once("://").unwrap_or(("*", value));
        let (domain, path) = rest.split_once('/').unwrap_or((rest, "*"));

        let (domain, port) = match rest.split_once(':') {
            Some((d, p)) => (d, Some(p)),
            None => (domain, None),
        };

        let mut domain = idna::domain_to_ascii(domain).ok()?;

        if let Some(port) = port {
            write!(&mut domain, ":{port}").unwrap();
        }

        Some(Self {
            scheme: scheme.to_string(),
            domain,
            path: path.to_string(),
        })
    }
}

/// Checks if the given `origin` matches one of the patterns in the `allowed` list.
///
/// Allowed origins may be defined in several ways:
/// - `http://domain.com[:port]`: Exact match for base URI (must include port).
/// - `*`: Allow any domain.
/// - `*.domain.com`: Matches domain.com and all subdomains, on any port.
/// - `domain.com`: Matches domain.com on any port.
/// - `*:port`: Wildcard on hostname, but explicit match on port.
///
/// This transparently handles [IDNA-encoded domains](https://en.wikipedia.org/wiki/Internationalized_domain_name).
pub fn is_valid_origin(origin: &Url, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return false;
    }

    // Two quick checks
    if allowed.iter().any(|elem| elem == "*") {
        return true;
    }

    if allowed.iter().any(|elem| elem == origin.as_str()) {
        return true;
    }

    let hostname = origin
        .host_str()
        .map(|host| idna::domain_to_ascii(host).unwrap_or(host.to_string()))
        .unwrap_or_default();

    let mut domain_matches = vec!["*".to_string(), hostname.clone()];
    if let Some(port) = origin.port_or_known_default() {
        domain_matches.push(format!("*:{port}"));
        domain_matches.push(format!("{hostname}:{port}"));
    }

    for pattern in allowed {
        let AllowedOriginPattern {
            scheme,
            domain,
            path,
        } = match AllowedOriginPattern::parse(pattern) {
            Some(pattern) => pattern,
            None => {
                tracing::warn!("Invalid allowed origin pattern: {pattern}");
                continue;
            }
        };

        // Match the scheme: wildcard or exact
        if scheme != "*" && scheme != origin.scheme() {
            continue;
        }

        // Match the domain: wildcard, prefix, or exact
        if let Some(rest) = domain.strip_prefix("*.") {
            if hostname.ends_with(&domain[1..]) || hostname == rest {
                return true;
            } else {
                continue;
            }
        } else if !domain_matches.contains(&domain) {
            continue;
        }

        // Match the path: wildcard, suffix, or exact
        if path == "*" {
            return true;
        }
        let path = path.strip_suffix('*').unwrap_or(&path);
        if origin.path().starts_with(path) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_untrusted_client() {
        symbolicator_test::setup();

        let server = symbolicator_test::Server::new();

        let result = create_client(&Default::default(), false, false) // untrusted
            .get(server.url("/"))
            .send()
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_untrusted_client_loopback() {
        symbolicator_test::setup();

        let server = symbolicator_test::Server::new();

        let mut url = server.url("/");
        url.set_host(Some("127.0.0.1")).unwrap();
        let result = create_client(&Default::default(), false, false) // untrusted
            .get(url)
            .send()
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_untrusted_client_allowed() {
        symbolicator_test::setup();

        let server = symbolicator_test::Server::new();

        let response = create_client(&Default::default(), true, false) // allowed to connect to reserved IPs
            .get(server.url("/garbage_data/OK"))
            .send()
            .await
            .unwrap();

        let text = response.text().await.unwrap();
        assert_eq!(text, "OK");
    }

    #[tokio::test]
    async fn test_client_redirect_policy() {
        let client = create_client(&Default::default(), false, false);

        let response = client
            .get("https://dev.azure.com/foo/bar.cs")
            .send()
            .await
            .unwrap();

        let status = response.status();

        assert_eq!(status.as_u16(), 302);
        assert!(status.is_redirection());
        assert!(!status.is_success());
    }

    fn is_valid_origin(origin: &str, allowed: &[&str]) -> bool {
        let allowed: Vec<_> = allowed.iter().map(|s| s.to_string()).collect();
        super::is_valid_origin(&origin.parse().unwrap(), &allowed)
    }

    #[test]
    fn test_global_wildcard_matches_domain() {
        assert!(is_valid_origin("http://example.com", &["*"]));
    }

    #[test]
    fn test_domain_wildcard_matches_domain() {
        assert!(is_valid_origin("http://example.com", &["*.example.com"]));
    }

    #[test]
    fn test_domain_wildcard_matches_domain_with_port() {
        assert!(is_valid_origin("http://example.com:80", &["*.example.com"]));
    }

    #[test]
    fn test_domain_wildcard_matches_subdomain() {
        assert!(is_valid_origin(
            "http://foo.example.com",
            &["*.example.com"]
        ));
    }

    #[test]
    fn test_domain_wildcard_matches_subdomain_with_port() {
        assert!(is_valid_origin(
            "http://foo.example.com:80",
            &["*.example.com"]
        ));
    }

    #[test]
    fn test_domain_wildcard_does_not_match_others() {
        assert!(!is_valid_origin("http://foo.com", &["*.example.com"]));
    }

    #[test]
    fn test_domain_wildcard_matches_domain_with_path() {
        assert!(is_valid_origin(
            "http://foo.example.com/foo/bar",
            &["*.example.com"]
        ));
    }

    #[test]
    fn test_base_domain_matches_domain() {
        assert!(is_valid_origin("http://example.com", &["example.com"]));
    }

    #[test]
    fn test_base_domain_matches_domain_with_path() {
        assert!(is_valid_origin(
            "http://example.com/foo/bar",
            &["example.com"]
        ));
    }

    #[test]
    fn test_base_domain_matches_domain_with_port() {
        assert!(is_valid_origin("http://example.com:80", &["example.com"]));
    }

    #[test]
    fn test_base_domain_matches_domain_with_explicit_port() {
        assert!(is_valid_origin(
            "http://example.com:80",
            &["example.com:80"]
        ));
    }

    #[test]
    fn test_base_domain_does_not_match_domain_with_invalid_port() {
        assert!(!is_valid_origin(
            "http://example.com:80",
            &["example.com:443"]
        ));
    }

    #[test]
    fn test_base_domain_does_not_match_subdomain() {
        assert!(!is_valid_origin("http://example.com", &["foo.example.com"]));
    }

    #[test]
    fn test_full_uri_match() {
        assert!(is_valid_origin(
            "http://example.com",
            &["http://example.com"]
        ));
    }

    #[test]
    fn test_full_uri_match_requires_scheme() {
        assert!(!is_valid_origin(
            "https://example.com",
            &["http://example.com"]
        ));
    }

    #[test]
    fn test_full_uri_match_does_not_require_port() {
        assert!(is_valid_origin(
            "http://example.com:80",
            &["http://example.com"]
        ));
    }

    #[test]
    fn test_partial_uri_match() {
        assert!(is_valid_origin(
            "http://example.com/foo/bar",
            &["http://example.com"]
        ));
    }

    #[test]
    fn test_custom_protocol_with_location() {
        assert!(is_valid_origin(
            "sp://custom-thing/foo/bar",
            &["sp://custom-thing"]
        ));
        assert!(!is_valid_origin(
            "sp://custom-thing-two/foo/bar",
            &["sp://custom-thing"]
        ));
    }

    #[test]
    fn test_custom_protocol_without_location() {
        assert!(is_valid_origin("sp://custom-thing/foo/bar", &["sp://*"]));
        assert!(!is_valid_origin("dp://custom-thing/foo/bar", &["sp://"]));
    }

    #[test]
    fn test_custom_protocol_with_domainish_match() {
        assert!(is_valid_origin(
            "sp://custom-thing.foobar/foo/bar",
            &["sp://*.foobar"]
        ));
        assert!(!is_valid_origin(
            "sp://custom-thing.bizbaz/foo/bar",
            &["sp://*.foobar"]
        ));
    }

    #[test]
    fn test_unicode() {
        assert!(is_valid_origin("http://løcalhost", &["*.løcalhost"]));
    }

    #[test]
    fn test_punycode() {
        assert!(is_valid_origin("http://xn--lcalhost-54a", &["*.løcalhost"]));
        assert!(is_valid_origin(
            "http://xn--lcalhost-54a",
            &["*.xn--lcalhost-54a"]
        ));
        assert!(is_valid_origin("http://løcalhost", &["*.xn--lcalhost-54a"]));
        assert!(is_valid_origin("http://xn--lcalhost-54a", &["løcalhost"]));
        assert!(is_valid_origin(
            "http://xn--lcalhost-54a:80",
            &["løcalhost:80"]
        ));
    }

    #[test]
    fn test_unparseable_uri() {
        assert!(!is_valid_origin("http://example.com", &["."]));
    }

    #[test]
    fn test_wildcard_hostname_with_port() {
        assert!(is_valid_origin("http://example.com:1234", &["*:1234"]));
    }

    #[test]
    fn test_without_hostname() {
        assert!(is_valid_origin("foo://", &["foo://*"]));
        assert!(is_valid_origin("foo://", &["foo://"]));
        assert!(!is_valid_origin("foo://", &["example.com"]));
        assert!(!is_valid_origin("foo://a", &["foo://"]));
        assert!(is_valid_origin("foo://a", &["foo://*"]));
    }
}
