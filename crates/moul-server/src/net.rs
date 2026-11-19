//! Calls out: one HTTPS client for webhooks, OAuth providers and mail APIs
//! that refuses private, local and link-local addresses, so a webhook
//! cannot be aimed at the server's own network or a cloud metadata service.
//!
//! The check runs on the addresses a name resolves to, and the connection
//! goes to an address that passed, so a name that changes between the check
//! and the call cannot slip through. Redirects are not followed, and proxy
//! settings in the environment are ignored. A server started with
//! `--allow-private-calls` (for tests and closed networks) skips the check
//! and uses the environment's proxy.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Whether `ip` is an address on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                // 100.64.0.0/10, shared address space (carrier NAT).
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                // 0.0.0.0/8 and 240.0.0.0/4.
                || o[0] == 0
                || o[0] >= 240
                // 192.0.0.0/24, protocol assignments.
                || (o[0] == 192 && o[1] == 0 && o[2] == 0))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                // 64:ff9b::/96 can reach IPv4 private ranges through NAT64.
                || (s[0] == 0x64 && s[1] == 0xff9b)
                // 2001:db8::/32, documentation.
                || (s[0] == 0x2001 && s[1] == 0xdb8))
        }
    }
}

struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = found.into_iter().filter(|a| is_public(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} resolves only to private or local addresses").into());
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// The client for calls out.
pub fn client(allow_private: bool) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("moul/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none());
    if !allow_private {
        // A proxy would resolve names itself, out of the guard's sight.
        b = b.dns_resolver(Arc::new(PublicOnly)).no_proxy();
    }
    b.build().expect("the HTTP client builds")
}

/// Checks a URL before a call: `http` or `https`, and, unless private
/// calls are allowed, not an address literal outside the public internet
/// (names are checked as they resolve).
pub fn check_url(url: &str, allow_private: bool) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(url).map_err(|e| format!("not a URL: {e}"))?;
    if u.scheme() != "https" && u.scheme() != "http" {
        return Err("only http and https URLs can be called".into());
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("a URL with credentials in it cannot be called".into());
    }
    if allow_private {
        return Ok(u);
    }
    let literal = match u.host() {
        Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d == "localhost"
                || d.ends_with(".localhost")
                || d.ends_with(".internal")
                || d.ends_with(".local")
            {
                return Err(format!("{d} is not a public address"));
            }
            None
        }
        None => return Err("the URL has no host".into()),
    };
    if let Some(ip) = literal
        && !is_public(ip)
    {
        return Err(format!("{ip} is not a public address"));
    }
    Ok(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses_are_refused() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public(bad.parse().unwrap()), "{bad}");
        }
        for good in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public(good.parse().unwrap()), "{good}");
        }
        assert!(check_url("http://169.254.169.254/latest", false).is_err());
        assert!(check_url("http://[::1]:8080/", false).is_err());
        assert!(check_url("http://localhost:9000/", false).is_err());
        assert!(check_url("https://user:pw@example.com/", false).is_err());
        assert!(check_url("ftp://example.com/", false).is_err());
        assert!(check_url("https://hooks.example.com/x", false).is_ok());
        assert!(check_url("http://127.0.0.1:9/x", true).is_ok());
    }
}
