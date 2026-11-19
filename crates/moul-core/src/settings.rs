//! Settings: what a native server reads to send mail, sign in with OAuth
//! providers, limit requests and keep logs. They live in the writer-only
//! store's `_settings`, by the Go engine's keys, so a converted Go database
//! keeps its configuration.
//!
//! Unlike the Go engine, the secret ones are never sent back: `GET
//! /api/settings` shows [`MASK`] for a secret that is set, and a `PATCH`
//! that sends the mask back leaves the secret as it was.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};
use serde_json::{Map, Value};

use crate::api::{ApiError, ApiResult, Response};
use crate::engine::Cx;

/// What a secret setting that is set reads as.
pub const MASK: &str = "********";

/// The rules a fresh server limits requests by: ten sign-in attempts every
/// three seconds and 300 requests every five, per address.
pub const DEFAULT_RATE_RULES: &str = r#"[{"label":"*:auth","maxRequests":10,"interval":3,"targetedUsers":"all"},{"label":"/","maxRequests":300,"interval":5,"targetedUsers":"all"}]"#;

/// Every setting and its default, as the Go engine seeds them.
pub const DEFAULTS: &[(&str, &str)] = &[
    ("app_name", "moul-dev"),
    ("email_api_key", ""),
    ("email_api_secret", ""),
    ("email_domain", ""),
    ("email_enabled", "false"),
    ("email_endpoint", ""),
    ("email_from_address", ""),
    ("email_from_name", ""),
    ("email_provider", "console"),
    ("email_region", "us-east-1"),
    ("file_s3_access_key", ""),
    ("file_s3_bucket", ""),
    ("file_s3_enabled", "false"),
    ("file_s3_endpoint", ""),
    ("file_s3_force_path_style", "false"),
    ("file_s3_region", ""),
    ("file_s3_secret_key", ""),
    ("file_thumbs_async", "false"),
    ("litestream_access_key_id", ""),
    ("litestream_enabled", "false"),
    ("litestream_replica_path", ""),
    ("litestream_s3_bucket", ""),
    ("litestream_s3_endpoint", ""),
    ("litestream_s3_force_path_style", "false"),
    ("litestream_s3_region", ""),
    ("litestream_secret_access_key", ""),
    ("oauth_apple_client_id", ""),
    ("oauth_apple_client_secret", ""),
    ("oauth_apple_enabled", "false"),
    ("oauth_apple_key_id", ""),
    ("oauth_apple_private_key", ""),
    ("oauth_apple_team_id", ""),
    ("oauth_github_client_id", ""),
    ("oauth_github_client_secret", ""),
    ("oauth_github_enabled", "false"),
    ("oauth_google_client_id", ""),
    ("oauth_google_client_secret", ""),
    ("oauth_google_enabled", "false"),
    ("oauth_redirect_url", ""),
    ("rate_limiting_enabled", "true"),
    ("rate_limiting_rules", DEFAULT_RATE_RULES),
    ("request_log_enabled", "true"),
    ("request_log_exclude_paths", ""),
    ("request_log_retention_days", "30"),
    ("request_log_sample_rate", "1"),
    ("root_user_allowed_ips", ""),
    ("root_user_ip_enabled", "false"),
    ("tls_domains", ""),
    ("tls_email", ""),
    ("tls_enabled", "false"),
    ("tls_http_port", "80"),
    ("tls_https_port", "443"),
    ("tls_use_staging", "false"),
    ("visit_retention_days", "30"),
];

/// The settings that hold secrets: never sent back.
pub const SECRETS: &[&str] = &[
    "email_api_key",
    "email_api_secret",
    "file_s3_secret_key",
    "litestream_secret_access_key",
    "oauth_apple_client_secret",
    "oauth_apple_private_key",
    "oauth_github_client_secret",
    "oauth_google_client_secret",
];

/// The settings, stored values over defaults. Keys this engine does not know
/// (a Go database may carry some) are left out.
pub fn load(conn: &Connection) -> rusqlite::Result<BTreeMap<String, String>> {
    let mut out: BTreeMap<String, String> = DEFAULTS
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let mut stmt = conn.prepare("SELECT key, value FROM writer._settings")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (k, v) = row?;
        if let Some(slot) = out.get_mut(&k) {
            *slot = v;
        }
    }
    Ok(out)
}

/// One setting, or its default.
pub fn get(conn: &Connection, key: &str) -> String {
    conn.query_row(
        "SELECT value FROM writer._settings WHERE key = ?1",
        [key],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .or_else(|| {
        DEFAULTS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_owned())
    })
    .unwrap_or_default()
}

/// Whether a boolean setting is `"true"`.
pub fn enabled(conn: &Connection, key: &str) -> bool {
    get(conn, key) == "true"
}

fn masked(all: BTreeMap<String, String>) -> Value {
    Value::Object(
        all.into_iter()
            .map(|(k, v)| {
                let v = if SECRETS.contains(&k.as_str()) && !v.is_empty() {
                    MASK.to_owned()
                } else {
                    v
                };
                (k, Value::String(v))
            })
            .collect::<Map<_, _>>(),
    )
}

/// Checks a value before it is stored, with the Go engine's messages for
/// the request log's settings.
fn validate(key: &str, value: &str) -> Result<(), String> {
    match key {
        "request_log_enabled" if value != "true" && value != "false" => {
            Err(format!("{key} must be true or false"))
        }
        "request_log_sample_rate" => match value.trim().parse::<f64>() {
            Ok(r) if (0.0..=1.0).contains(&r) => Ok(()),
            _ => Err(format!("{key} must be a number from 0 to 1")),
        },
        "request_log_retention_days" | "visit_retention_days" => {
            match value.trim().parse::<i64>() {
                Ok(d) if d >= 1 => Ok(()),
                _ => Err(format!("{key} must be a whole number of days, at least 1")),
            }
        }
        // The Go engine kept the old rules and answered 200 when these did
        // not parse; a rule that silently does nothing is worse.
        "rate_limiting_rules" => {
            let ok = serde_json::from_str::<Vec<Value>>(value).is_ok_and(|rules| {
                rules.iter().all(|r| {
                    r["label"].as_str().is_some_and(|l| !l.is_empty())
                        && r["maxRequests"].as_f64().is_some_and(|n| n >= 1.0)
                        && r["interval"].as_f64().is_some_and(|n| n > 0.0)
                })
            });
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "{key} must be a JSON array of rules, each with a label, maxRequests of at least 1 and an interval in seconds above 0"
                ))
            }
        }
        _ => Ok(()),
    }
}

/// `GET /api/settings`: every setting, secrets masked. Root users and the
/// owner.
pub(crate) fn get_endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    Ok(Response::json(200, masked(load(cx.conn)?)))
}

/// `PATCH /api/settings`: changes the settings it names. Values must be
/// strings; keys this engine does not know are ignored, as in Go.
pub(crate) fn patch_endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let body = match cx.req.json_object() {
        Ok(Some(m)) => m,
        Ok(None) => Map::new(),
        Err(()) => return Err(ApiError::bad_request("Invalid JSON payload")),
    };
    let mut changes = Vec::new();
    for (k, v) in &body {
        let Some(v) = v.as_str() else {
            return Err(ApiError::bad_request("Invalid JSON payload"));
        };
        validate(k, v).map_err(ApiError::bad_request)?;
        if !DEFAULTS.iter().any(|(d, _)| d == k) {
            continue;
        }
        // The mask comes back from a form that showed it: no change.
        if SECRETS.contains(&k.as_str()) && v == MASK {
            continue;
        }
        changes.push((k.clone(), v.to_owned()));
    }
    for (k, v) in changes {
        cx.conn.execute(
            "INSERT INTO writer._settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![k, v],
        )?;
    }
    Ok(Response::json(200, masked(load(cx.conn)?)))
}

/// `POST /api/settings/reload` and `/api/admin/reload`. The engine reads
/// settings when it needs them; the host reloads what it caches when it
/// sees this answer.
pub(crate) fn reload_endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin_key()?;
    Ok(Response::json(
        200,
        serde_json::json!({"status": "ok", "message": "Settings reloaded successfully"}),
    ))
}

/// Whether `ip` is in `list`: addresses and CIDR ranges separated by commas.
/// An empty list allows every address, as in the Go engine.
pub fn ip_allowed(list: &str, ip: &str) -> bool {
    let entries: Vec<&str> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if entries.is_empty() {
        return true;
    }
    let Ok(addr) = ip.trim().parse::<std::net::IpAddr>() else {
        return false;
    };
    entries.iter().any(|e| match e.split_once('/') {
        Some((net, bits)) => match (net.parse::<std::net::IpAddr>(), bits.parse::<u32>()) {
            (Ok(net), Ok(bits)) => in_cidr(addr, net, bits),
            _ => false,
        },
        None => e.parse::<std::net::IpAddr>().is_ok_and(|a| a == addr),
    })
}

fn in_cidr(addr: std::net::IpAddr, net: std::net::IpAddr, bits: u32) -> bool {
    use std::net::IpAddr;
    match (addr, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) if bits <= 32 => {
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            u32::from(a) & mask == u32::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(n)) if bits <= 128 => {
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            u128::from(a) & mask == u128::from(n) & mask
        }
        // An IPv4 address seen as IPv6 (`::ffff:a.b.c.d`).
        (IpAddr::V6(a), IpAddr::V4(_)) => a
            .to_ipv4_mapped()
            .is_some_and(|a4| in_cidr(IpAddr::V4(a4), net, bits)),
        _ => false,
    }
}

/// Whether a root user may act from `remote`: the allowlist is off, or the
/// address is on it. An unknown address passes only when the list is off.
pub fn root_ip_allowed(conn: &Connection, remote: Option<&str>) -> bool {
    if !enabled(conn, "root_user_ip_enabled") {
        return true;
    }
    let list = get(conn, "root_user_allowed_ips");
    match remote {
        Some(ip) => ip_allowed(&list, ip),
        None => list.trim().is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlists_match_addresses_and_ranges() {
        assert!(ip_allowed("", "1.2.3.4"));
        assert!(ip_allowed("1.2.3.4", "1.2.3.4"));
        assert!(!ip_allowed("1.2.3.4", "1.2.3.5"));
        assert!(ip_allowed("10.0.0.0/8, 192.168.1.1", "10.20.30.40"));
        assert!(ip_allowed("10.0.0.0/8", "::ffff:10.1.1.1"));
        assert!(ip_allowed("2001:db8::/32", "2001:db8::1"));
        assert!(!ip_allowed("2001:db8::/32", "2001:db9::1"));
        assert!(!ip_allowed("10.0.0.0/8", "not an address"));
    }

    #[test]
    fn rules_must_parse() {
        assert!(validate("rate_limiting_rules", DEFAULT_RATE_RULES).is_ok());
        assert!(validate("rate_limiting_rules", "[{").is_err());
        assert!(
            validate(
                "rate_limiting_rules",
                r#"[{"label":"/","maxRequests":5,"interval":0}]"#
            )
            .is_err()
        );
        assert!(validate("request_log_sample_rate", "1.5").is_err());
        assert!(validate("request_log_sample_rate", "0.25").is_ok());
        assert!(validate("visit_retention_days", "0").is_err());
    }
}
