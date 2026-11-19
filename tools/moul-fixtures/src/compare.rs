//! Comparing an answer with the one the Go engine recorded, for the parity
//! suites of the engine and the server.
//!
//! Ids, tokens and timestamps differ between two runs, so the first time a
//! Go value of that kind meets a Rust value at the same place in an answer,
//! the pair is remembered ([`Subs`]) and later requests are rewritten with
//! the Rust value. Everything else must match: keys and values.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::{Body, HttpFixture};

/// The time the parity suites run at.
pub const NOW: i64 = 1_794_961_964_000;

/// The secret the Go handler tests sign their own tokens with.
pub const GO_TEST_JWT_SECRET: &[u8] = b"test-secret-key-for-unit-tests-1234";

/// The admin key the Go handler security tests configure.
pub const GO_SECURITY_ADMIN_KEY: &str = "security-test-admin-key";

/// Keys whose values are timings, not data.
pub const TIMING_KEYS: &[&str] = &["duration_us", "client_id_issued_at"];

/// Go values and the Rust values that stand for them.
#[derive(Default)]
pub struct Subs {
    /// Go value → Rust value.
    pub map: BTreeMap<String, String>,
    /// Go tokens that stand for the owner (root users, admin login).
    pub owner_tokens: Vec<String>,
}

impl Subs {
    /// `s` with every Go value replaced by its Rust value.
    pub fn apply(&self, s: &str) -> String {
        let mut out = s.to_owned();
        let mut keys: Vec<&String> = self.map.keys().collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
        for k in keys {
            if out.contains(k.as_str()) {
                out = out.replace(k.as_str(), &self.map[k]);
            }
        }
        out
    }
}

fn is_alnum(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Whether `s` is a value that differs between runs by nature.
pub fn is_dynamic(s: &str) -> bool {
    let b = s.as_bytes();
    let timestamp =
        b.len() >= 20 && b[4] == b'-' && b[7] == b'-' && (b[10] == b'T' || b[10] == b' ');
    let jwt = s.split('.').count() == 3 && s.starts_with("eyJ");
    let id15 = s.len() == 15 && is_alnum(s);
    let record_id = s
        .rsplit_once('-')
        .is_some_and(|(p, r)| r.len() == 15 && is_alnum(r) && !p.is_empty());
    let storage = s.starts_with("/storage/");
    let hex = s.len() >= 32 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let field_id = s.starts_with("fld_");
    let stamp_name = s.contains("-records-") && (s.ends_with(".csv") || s.ends_with(".json"));
    let flag_id = s
        .strip_prefix("ff_")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    // The server's OAuth clients, secrets, tickets, codes and tokens.
    let prefixed = ["moul_client_", "moul_sec_", "tkt_", "mcp-session-"]
        .iter()
        .any(|p| s.strip_prefix(p).is_some_and(|r| r.len() >= 12));
    let long_alnum = s.len() >= 20 && is_alnum(s);
    let user_code = s.len() == 9
        && s.as_bytes()[4] == b'-'
        && s.bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || b.is_ascii_uppercase() || b.is_ascii_digit());
    timestamp
        || jwt
        || id15
        || record_id
        || storage
        || hex
        || field_id
        || stamp_name
        || flag_id
        || prefixed
        || long_alnum
        || user_code
}

/// Compares a Go value with a Rust value, learning substitutions.
pub fn compare(go: &Value, rs: &Value, subs: &mut Subs, at: &str) -> Result<(), String> {
    let mut first = None;
    walk(go, rs, subs, at, &mut first);
    match first {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn walk_err(first: &mut Option<String>, e: String) {
    if first.is_none() {
        *first = Some(e);
    }
}

/// Walks the whole value, learning every substitution, and keeps the first
/// difference.
fn walk(go: &Value, rs: &Value, subs: &mut Subs, at: &str, first: &mut Option<String>) {
    if let Err(e) = compare_here(go, rs, subs, at, first)
        && first.is_none()
    {
        *first = Some(e);
    }
}

fn compare_here(
    go: &Value,
    rs: &Value,
    subs: &mut Subs,
    at: &str,
    first: &mut Option<String>,
) -> Result<(), String> {
    match (go, rs) {
        (Value::Object(g), Value::Object(r)) => {
            for (k, gv) in g {
                let here = format!("{at}.{k}");
                if TIMING_KEYS.contains(&k.as_str()) {
                    continue;
                }
                match r.get(k) {
                    Some(rv) => walk(gv, rv, subs, &here, first),
                    // Go writes nil slices as null where Rust leaves the key out.
                    None if gv.is_null() => {}
                    None => walk_err(first, format!("{here}: missing (Go has {gv})")),
                }
            }
            for k in r.keys() {
                if !g.contains_key(k) && !(k == "id" && at.contains(".fields[")) {
                    walk_err(first, format!("{at}.{k}: extra (Rust has {})", r[k]));
                }
            }
            Ok(())
        }
        (Value::Array(g), Value::Array(r)) => {
            if g.len() != r.len() {
                return Err(format!(
                    "{at}: {} items in Go, {} in Rust",
                    g.len(),
                    r.len()
                ));
            }
            for (i, (gv, rv)) in g.iter().zip(r).enumerate() {
                walk(gv, rv, subs, &format!("{at}[{i}]"), first);
            }
            Ok(())
        }
        // Go writes an empty fields list as null.
        (Value::Null, Value::Array(r)) if r.is_empty() => Ok(()),
        (Value::String(g), Value::String(r)) => {
            if g == r || loopback(g) == loopback(r) && loopback(g) != *g {
                return Ok(());
            }
            let expected = subs.apply(g);
            if &expected == r || loopback(&expected) == loopback(r) {
                return Ok(());
            }
            if is_dynamic(g) && is_dynamic(r) {
                if let Some(known) = subs.map.get(g)
                    && known != r
                {
                    return Err(format!(
                        "{at}: Go {g:?} was Rust {known:?} before, now {r:?}"
                    ));
                }
                subs.map.insert(g.clone(), r.clone());
                // A signed URL also names the bare one.
                if let (Some((gb, gq)), Some((rb, rq))) = (g.split_once('?'), r.split_once('?')) {
                    subs.map.insert(gb.to_owned(), rb.to_owned());
                    subs.map.insert(gq.to_owned(), rq.to_owned());
                }
                return Ok(());
            }
            Err(format!("{at}: Go {g:?}, Rust {r:?}"))
        }
        (Value::Number(g), Value::Number(r)) if g.as_f64() == r.as_f64() => Ok(()),
        _ if go == rs => Ok(()),
        _ => Err(format!("{at}: Go {go}, Rust {rs}")),
    }
}

/// A recorded body's bytes.
pub fn body_bytes(b: &Option<Body>) -> Option<Vec<u8>> {
    let b = b.as_ref()?;
    if let Some(j) = &b.json {
        return Some(serde_json::to_vec(j).unwrap());
    }
    if let Some(t) = &b.text {
        return Some(t.clone().into_bytes());
    }
    b.base64.as_ref().map(|s| crate::decode_base64(s))
}

/// The admin key a recorded test used.
pub fn admin_key(fx: &HttpFixture) -> Option<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for s in &fx.sessions {
        for ex in &s.exchanges {
            if ex.response.status < 300
                && let Some(v) = ex
                    .request
                    .headers
                    .get("X-Admin-Key")
                    .and_then(|v| v.first())
            {
                *counts.entry(v).or_default() += 1;
            }
        }
    }
    if let Some((k, _)) = counts.into_iter().max_by_key(|(_, n)| *n) {
        return Some(k.to_owned());
    }
    // A test whose every answer was a refusal of the body (400) still
    // got past the key check.
    for s in &fx.sessions {
        for ex in &s.exchanges {
            if ex.response.status == 400
                && let Some(v) = ex
                    .request
                    .headers
                    .get("X-Admin-Key")
                    .and_then(|v| v.first())
            {
                return Some(v.clone());
            }
        }
    }
    // Handler tests that never send the key in a header still configure
    // the one the security tests share.
    Some(GO_SECURITY_ADMIN_KEY.to_owned())
}

/// A recorded body as JSON, if it is JSON.
pub fn expected_json(b: &Option<Body>) -> Option<Value> {
    let b = b.as_ref()?;
    if let Some(j) = &b.json {
        return Some(j.clone());
    }
    b.text.as_ref().and_then(|t| serde_json::from_str(t).ok())
}

/// `s` with every `http://127.0.0.1:<port>` written without its port: a
/// recorded test served on a random port.
pub fn loopback(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    const ORIGIN: &str = "http://127.0.0.1:";
    while let Some(i) = rest.find(ORIGIN) {
        out.push_str(&rest[..i + ORIGIN.len() - 1]);
        let after = &rest[i + ORIGIN.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}
