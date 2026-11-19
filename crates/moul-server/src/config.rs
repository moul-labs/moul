//! How a server is set up: flags and the environment, as the Go engine's
//! `MOUL_*` variables name them.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

/// Settings of a native server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The database folder: `main.db`, `writer.db`, `device.db`, `files/`
    /// and `ltx/`.
    pub dir: PathBuf,
    /// Where to listen.
    pub listen: SocketAddr,
    /// The address people reach the server at, for links in mail and OAuth
    /// redirects. Defaults to `http://<listen>`.
    pub public_url: String,
    /// The admin key (`X-Admin-Key`). `None` keeps one in the writer-only
    /// store, made on first start.
    pub admin_key: Option<String>,
    /// The key that signs tokens. A Go engine's `MOUL_JWT_SECRET`, so its
    /// tokens stay valid; `None` keeps one in the writer-only store.
    pub signing_key: Option<Vec<u8>>,
    /// Origins allowed to call from a browser. Empty allows any (`*`), as
    /// the Go engine does.
    pub cors_origins: Vec<String>,
    /// Proxies whose `X-Forwarded-For` is believed.
    pub trusted_proxies: Vec<Trusted>,
    /// A header that carries the client's address, set by a trusted proxy.
    pub client_ip_header: Option<String>,
    /// How long a sign-in token lasts.
    pub token_ttl_ms: i64,
    /// How long after expiry a token may be refreshed.
    pub refresh_grace_ms: i64,
    /// How long a signed file URL lasts.
    pub file_url_ttl_ms: i64,
    /// The largest request body, in bytes.
    pub max_body_bytes: usize,
    /// Where to push the database as a Litestream-compatible replica: a
    /// folder or an `s3://` URL. Also taken from the `litestream_*` settings.
    pub replica: Option<String>,
    /// The name MCP clients see. Defaults to the `app_name` setting.
    pub app_name: Option<String>,
    /// Send mail and webhook calls, and run jobs. Off for a read-only copy.
    pub effects: bool,
    /// Serve a database another writer owns, read-only (a folder `moul
    /// pull` keeps).
    pub follower: bool,
    /// Let webhooks and jobs call private and local addresses: for tests
    /// and closed networks.
    pub allow_private_calls: bool,
    /// A database file to fill a new folder from: a Moul export or a Go
    /// engine database, converted.
    pub import: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            dir: PathBuf::from("moul-data"),
            listen: SocketAddr::from(([127, 0, 0, 1], 8090)),
            public_url: String::new(),
            admin_key: None,
            signing_key: None,
            cors_origins: Vec::new(),
            trusted_proxies: vec![Trusted::Loopback],
            client_ip_header: None,
            token_ttl_ms: 24 * 3600 * 1000,
            refresh_grace_ms: 7 * 24 * 3600 * 1000,
            file_url_ttl_ms: 3600 * 1000,
            max_body_bytes: 33 << 20,
            replica: None,
            app_name: None,
            effects: true,
            follower: false,
            allow_private_calls: false,
            import: None,
        }
    }
}

impl ServerConfig {
    /// The public address, or `http://<listen>` when none was given.
    pub fn public_url(&self) -> String {
        if self.public_url.is_empty() {
            format!("http://{}", self.listen)
        } else {
            self.public_url.trim_end_matches('/').to_owned()
        }
    }
}

/// Proxies a server believes about the client's address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trusted {
    /// 127.0.0.0/8 and ::1.
    Loopback,
    /// Loopback, link-local and private ranges.
    Private,
    /// One address or range.
    Net(IpAddr, u8),
}

impl Trusted {
    /// Whether `ip` is trusted.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        match self {
            Trusted::Loopback => ip.is_loopback(),
            Trusted::Private => {
                ip.is_loopback()
                    || match ip {
                        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
                        IpAddr::V6(v6) => {
                            (v6.segments()[0] & 0xfe00) == 0xfc00
                                || (v6.segments()[0] & 0xffc0) == 0xfe80
                        }
                    }
            }
            Trusted::Net(net, bits) => {
                moul_core::settings::ip_allowed(&format!("{net}/{bits}"), &ip.to_string())
            }
        }
    }
}

/// An IPv4 address seen through IPv6 as the IPv4 address.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// Reads `MOUL_TRUSTED_PROXIES`: addresses, ranges, `loopback`, `private`
/// or `none`, separated by commas. Unset means `loopback`.
pub fn parse_trusted(list: &str) -> Result<Vec<Trusted>, String> {
    let mut out = Vec::new();
    for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match item {
            "loopback" => out.push(Trusted::Loopback),
            "private" => out.push(Trusted::Private),
            "none" => {}
            _ => {
                let (addr, bits) = match item.split_once('/') {
                    Some((a, b)) => (a, Some(b)),
                    None => (item, None),
                };
                let ip: IpAddr = addr.parse().map_err(|_| {
                    format!("MOUL_TRUSTED_PROXIES: {item:?} is not an address or range")
                })?;
                let max = if ip.is_ipv4() { 32 } else { 128 };
                let bits = match bits {
                    Some(b) => b.parse::<u8>().ok().filter(|b| *b <= max).ok_or_else(|| {
                        format!("MOUL_TRUSTED_PROXIES: {item:?} has a bad prefix")
                    })?,
                    None => max,
                };
                out.push(Trusted::Net(ip, bits));
            }
        }
    }
    Ok(out)
}

/// Reads a duration such as `24h`, `30m`, `90s` or `500ms`, or a number of
/// seconds, as Go's `time.ParseDuration` reads the common cases.
pub fn parse_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<i64>() {
        return Some(secs * 1000);
    }
    let mut total = 0f64;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let mut unit = String::from(c);
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphabetic() {
                unit.push(n);
                chars.next();
            } else {
                break;
            }
        }
        let v: f64 = num.parse().ok()?;
        num.clear();
        let ms = match unit.as_str() {
            "ms" => 1.0,
            "s" => 1000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            _ => return None,
        };
        total += v * ms;
        any = true;
    }
    (any && num.is_empty()).then_some(total as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_proxies_parse() {
        assert_eq!(parse_duration_ms("24h"), Some(86_400_000));
        assert_eq!(parse_duration_ms("1h30m"), Some(5_400_000));
        assert_eq!(parse_duration_ms("90"), Some(90_000));
        assert_eq!(parse_duration_ms("5x"), None);
        let t = parse_trusted("loopback, 10.0.0.0/8").unwrap();
        assert!(t[0].contains("127.0.0.1".parse().unwrap()));
        assert!(t[1].contains("10.1.2.3".parse().unwrap()));
        assert!(!t[1].contains("11.1.2.3".parse().unwrap()));
        assert!(parse_trusted("nope").is_err());
        assert!(parse_trusted("none").unwrap().is_empty());
    }
}
