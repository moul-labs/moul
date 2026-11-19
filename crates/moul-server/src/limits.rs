//! Rate limits and live-stream caps, as the Go engine applies them.
//!
//! Limits come from the settings: `rate_limiting_enabled` and
//! `rate_limiting_rules`, a list of `{label, maxRequests, interval,
//! targetedUsers, maxConcurrent?}`. The first rule that matches a request
//! applies. A label with a colon is `<collection>:<action>` (`*` matches
//! any), where the action is `auth`, `list`, `view`, `create`, `update`,
//! `delete`, `subscribe` or `ticket`; any other label is a path prefix. Each
//! address has a bucket of `maxRequests` per rule that refills over
//! `interval` seconds. The admin key is never limited.
//!
//! Live streams are capped too: 30 at a time from one address and 15 for
//! one person, plus a rule's `maxConcurrent`.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use serde::Deserialize;

use crate::http::{AppState, engine_headers, error};

/// Live streams one address may hold.
pub const MAX_STREAMS_PER_IP: usize = 30;
/// Live streams one person may hold.
pub const MAX_STREAMS_PER_USER: usize = 15;

/// One rule.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// `<collection>:<action>` or a path prefix.
    pub label: String,
    /// Requests per interval.
    pub max_requests: f64,
    /// The interval, in seconds.
    pub interval: f64,
    /// `all`, `authenticated` or `guest`.
    #[serde(default)]
    pub targeted_users: String,
    /// Live streams at a time, for subscribe rules.
    #[serde(default)]
    pub max_concurrent: usize,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

#[derive(Default)]
struct State {
    enabled: bool,
    rules: Vec<Rule>,
    buckets: HashMap<String, Bucket>,
    swept: Option<Instant>,
}

/// The limiter.
#[derive(Default)]
pub struct Limiter {
    state: Mutex<State>,
    streams: Mutex<HashMap<String, usize>>,
}

/// What a request is, for rules with a colon.
fn moul_action(method: &str, path: &str) -> Option<(String, &'static str)> {
    let p = path.trim_end_matches('/');
    if p == "/api/moul/subscribe" {
        return Some(("*".into(), "subscribe"));
    }
    if p == "/api/realtime/ticket" {
        return Some(("*".into(), "ticket"));
    }
    // Paths that check a root user's password, or hand out codes and
    // tokens, count as signing in.
    if method == "POST"
        && matches!(
            p,
            "/api/oauth2/authorize"
                | "/api/oauth2/token"
                | "/api/oauth2/register"
                | "/api/oauth2/device/authorize"
                | "/api/oauth2/device/token"
                | "/device/verify"
                | "/api/admin/login"
        )
    {
        return Some(("_rootUsers".into(), "auth"));
    }
    if method == "POST" && matches!(p, "/api/hub/signin" | "/api/hub/verify" | "/api/hub/join") {
        return Some(("accounts".into(), "auth"));
    }
    let rest = p.strip_prefix("/api/moul/")?;
    let segs: Vec<&str> = rest.split('/').collect();
    let name = segs.first()?.to_string();
    if segs.len() < 2 {
        return None;
    }
    if rest.contains("/auth-with-") || rest.contains("/otp/") || rest.contains("/passkey/") {
        return Some((name, "auth"));
    }
    match (method, &segs[1..]) {
        ("POST", ["records"]) => Some((name, "create")),
        ("GET", ["records"]) => Some((name, "list")),
        ("GET", ["records", _]) => Some((name, "view")),
        ("PATCH", ["records", _]) => Some((name, "update")),
        ("DELETE", ["records", _]) => Some((name, "delete")),
        (_, ["subscribe"]) => Some((name, "subscribe")),
        _ => None,
    }
}

fn wild(pattern: &str, value: &str) -> bool {
    pattern == "*" || pattern == value
}

impl Limiter {
    /// Applies the settings.
    pub fn configure(&self, settings: &BTreeMap<String, String>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.enabled = settings
            .get("rate_limiting_enabled")
            .is_some_and(|v| v == "true");
        if let Some(rules) = settings
            .get("rate_limiting_rules")
            .and_then(|r| serde_json::from_str::<Vec<Rule>>(r).ok())
        {
            s.rules = rules
                .into_iter()
                .filter(|r| r.max_requests >= 1.0 && r.interval > 0.0)
                .collect();
        }
        s.buckets.clear();
    }

    /// The first rule for a request, if limits are on.
    fn rule(&self, method: &str, path: &str, signed_in: Option<bool>) -> Option<Rule> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.enabled {
            return None;
        }
        let ma = moul_action(method, path);
        s.rules
            .iter()
            .find(|r| {
                let users_ok = match r.targeted_users.as_str() {
                    "authenticated" => signed_in == Some(true),
                    "guest" => signed_in == Some(false),
                    _ => true,
                };
                users_ok
                    && match r.label.split_once(':') {
                        Some((m, a)) => ma
                            .as_ref()
                            .is_some_and(|(name, action)| wild(m, name) && wild(a, action)),
                        None => path.starts_with(&r.label),
                    }
            })
            .cloned()
    }

    fn needs_identity(&self) -> bool {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.enabled
            && s.rules
                .iter()
                .any(|r| r.targeted_users == "authenticated" || r.targeted_users == "guest")
    }

    /// Takes one token for the request, or refuses it with 429.
    pub async fn check(
        &self,
        state: &AppState,
        method: &str,
        path: &str,
        headers: &HeaderMap,
        ip: IpAddr,
    ) -> Option<Response> {
        if has_admin_key(headers, &state.admin_key) {
            return None;
        }
        let signed_in = if self.needs_identity() {
            let creds = engine_headers(headers);
            let remote = ip.to_string();
            state
                .db
                .run(move |db| db.engine.identity(&creds, Some(&remote)))
                .await
                .ok()
                .and_then(Result::ok)
                .map(|id| id.user.is_some() || id.admin)
        } else {
            None
        };
        let rule = self.rule(method, path, signed_in)?;
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if s.swept.is_none_or(|t| now - t > Duration::from_secs(60)) {
            s.buckets
                .retain(|_, b| now - b.at < Duration::from_secs(600));
            s.swept = Some(now);
        }
        let b = s
            .buckets
            .entry(format!("{}:{}", network(ip), rule.label))
            .or_insert(Bucket {
                tokens: rule.max_requests,
                at: now,
            });
        let rate = rule.max_requests / rule.interval;
        b.tokens = (b.tokens + (now - b.at).as_secs_f64() * rate).min(rule.max_requests);
        b.at = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            return None;
        }
        let wait = ((1.0 - b.tokens) / rate).ceil().max(1.0) as u64;
        let mut r = error(429, "Too many requests");
        if let Ok(v) = HeaderValue::try_from(wait.to_string()) {
            r.headers_mut().insert(header::RETRY_AFTER, v);
        }
        Some(r)
    }

    /// Counts a live stream in, or says why it is refused. `user` is the
    /// signed-in person; admins are not counted. The guard counts it out.
    pub fn open_stream(
        &self,
        ip: IpAddr,
        user: Option<&str>,
        admin: bool,
        collection: &str,
        path: &str,
    ) -> Result<StreamSlot<'_>, &'static str> {
        let enabled = self.state.lock().unwrap_or_else(|e| e.into_inner()).enabled;
        if admin || !enabled {
            return Ok(StreamSlot {
                limiter: self,
                keys: Vec::new(),
            });
        }
        let mut keys = vec![(
            format!("ip:{}", network(ip)),
            MAX_STREAMS_PER_IP,
            "Too many concurrent realtime connections for this IP",
        )];
        if let Some(u) = user {
            keys.push((
                format!("user:{u}"),
                MAX_STREAMS_PER_USER,
                "Too many concurrent realtime connections for this user",
            ));
        }
        let rule = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.rules
                .iter()
                .find(|r| {
                    r.max_concurrent > 0
                        && match r.label.split_once(':') {
                            Some((m, a)) => wild(m, collection) && wild(a, "subscribe"),
                            None => path.starts_with(&r.label),
                        }
                })
                .cloned()
        };
        if let Some(r) = rule {
            let who = user.map_or_else(|| format!("ip:{}", network(ip)), |u| format!("user:{u}"));
            keys.push((
                format!("{who}:{}", r.label),
                r.max_concurrent,
                "Too many concurrent realtime connections for this subscription",
            ));
        }
        let mut counts = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        for (k, max, why) in &keys {
            if counts.get(k).copied().unwrap_or(0) >= *max {
                return Err(why);
            }
        }
        for (k, _, _) in &keys {
            *counts.entry(k.clone()).or_default() += 1;
        }
        Ok(StreamSlot {
            limiter: self,
            keys: keys.into_iter().map(|(k, _, _)| k).collect(),
        })
    }
}

/// A live stream's place in the counts, given back when dropped.
pub struct StreamSlot<'a> {
    limiter: &'a Limiter,
    keys: Vec<String>,
}

impl StreamSlot<'_> {
    /// The keys, so the slot can outlive a borrow of the limiter.
    pub fn into_keys(mut self) -> Vec<String> {
        std::mem::take(&mut self.keys)
    }
}

impl Drop for StreamSlot<'_> {
    fn drop(&mut self) {
        self.limiter.release(&self.keys);
    }
}

impl Limiter {
    /// Counts streams out.
    pub fn release(&self, keys: &[String]) {
        let mut counts = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        for k in keys {
            if let Some(n) = counts.get_mut(k) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    counts.remove(k);
                }
            }
        }
    }
}

/// The network a client counts as: its IPv4 address, or its IPv6 /64, so
/// that one host cannot step around a limit by changing its address.
pub fn network(ip: IpAddr) -> String {
    match crate::config::canonical(ip) {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        v4 => v4.to_string(),
    }
}

/// Whether a request carries the admin key, as the engine reads it.
pub fn has_admin_key(headers: &HeaderMap, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut r = moul_core::Request::new("GET", "/");
    r.headers = engine_headers(headers);
    let eq = |s: &str| moul_core::crypto::constant_time_eq(s.as_bytes(), key.as_bytes());
    let x = r.header_value("X-Admin-Key");
    if !x.is_empty() && eq(x) {
        return true;
    }
    let auth = r.header_value("Authorization");
    let token = auth
        .split_once(' ')
        .filter(|(s, _)| s.eq_ignore_ascii_case("bearer"))
        .map_or(auth, |(_, t)| t);
    !token.is_empty() && eq(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_match_actions_and_prefixes() {
        let l = Limiter::default();
        let mut s = BTreeMap::new();
        s.insert("rate_limiting_enabled".to_owned(), "true".to_owned());
        s.insert(
            "rate_limiting_rules".to_owned(),
            moul_core::settings::DEFAULT_RATE_RULES.to_owned(),
        );
        l.configure(&s);
        let r = l
            .rule("POST", "/api/moul/users/auth-with-password", None)
            .unwrap();
        assert_eq!(r.label, "*:auth");
        let r = l.rule("GET", "/api/moul/posts/records", None).unwrap();
        assert_eq!(r.label, "/");
        assert_eq!(
            moul_action("GET", "/api/moul/posts/records/p1"),
            Some(("posts".into(), "view"))
        );
        assert_eq!(
            moul_action("GET", "/api/moul/subscribe"),
            Some(("*".into(), "subscribe"))
        );
    }
}
