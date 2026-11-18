//! The parity suite: replays the HTTP traffic recorded from the Go engine's
//! own tests against this engine and compares the answers.
//!
//! Each recorded session starts from the database state the Go test had
//! before its first request, converted on open. Requests are sent in order.
//! Ids, tokens and timestamps differ between the two runs, so the first time
//! a Go value of that kind meets a Rust value at the same place in a
//! response, the pair is remembered and later requests are rewritten with
//! the Rust value. Everything else must match: status, keys and values.
//!
//! Exchanges on routes this engine does not serve in the first browser
//! release (setup, settings, feature flags, workers, OAuth, MCP, server-sent
//! events, logs, metrics) are skipped. A root user token those routes issued
//! is replayed as the owner. A difference that is a decision, not a bug, is
//! listed in `DECIDED` with its reason.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;

use moul_core::{
    Body, Caller, Config, Engine, MemoryBlobs, Request, Response, ResponseBody, SeededHost,
};
use moul_fixtures::{Exchange, Fixtures, HttpFixture};
use moul_store::Store;
use serde_json::Value;

/// Echo route patterns this engine serves.
const IN_SCOPE: &[&str] = &[
    "/api/moul",
    "/api/moul/:name",
    "/api/moul/:name/records",
    "/api/moul/:name/records/:id",
    "/api/moul/:name/export",
    "/api/moul/:name/import",
    "/api/moul/:name/email-templates",
    "/api/moul/:name/auth-with-password",
    "/api/moul/:name/auth-with-otp",
    "/api/moul/:name/otp/request",
    "/api/moul/:name/refresh",
    "/api/moul/:name/auth-refresh",
    "/api/moul/:name/logout",
    "/api/moul/:name/request-password-reset",
    "/api/moul/:name/confirm-password-reset",
    "/api/moul/:name/auth-methods",
    "/api/upload",
    "/api/upload/*",
    "/api/files",
    "/api/files/*",
    "/storage/*",
    "/api/rules/test",
];

/// Differences that are decisions, or that the fixtures cannot reproduce.
/// Key: `<test>#<session>.<exchange>`; see `crates/moul-core/README.md`.
const DECIDED: &[(&str, &str)] = &[
    // Image renditions (thumbhash, thumbs) come after the first release.
    (
        "TestFiles_FieldValuesMustBeOwnUploads#0.0",
        "no thumbnails yet",
    ),
    (
        "TestFiles_PrivateRecordFilesNeedASignature#0.0",
        "no thumbnails yet",
    ),
    (
        "TestFiles_PrivateRecordFilesNeedASignature#0.10",
        "no thumbnails yet",
    ),
    (
        "TestFiles_PublicDecisionFollowsChanges#0.0",
        "no thumbnails yet",
    ),
    (
        "TestFiles_PublicDecisionFollowsChanges#0.5",
        "no thumbnails yet",
    ),
    (
        "TestFiles_PublicRecordFilesAreServedUnsigned#0.0",
        "no thumbnails yet",
    ),
    // A taken collection name is the caller's mistake: 400, not Go's 500.
    (
        "TestAdminSurface_RootOrAdminKeyOnly#0.7",
        "duplicate name answers 400",
    ),
    // The Go test created this collection straight in its database mid-test,
    // which the recording cannot see.
    (
        "TestFiles_PublicDecisionFollowsChanges#0.6",
        "made outside the API",
    ),
    (
        "TestFiles_PublicDecisionFollowsChanges#0.9",
        "made outside the API",
    ),
    // Likewise the file_allowed_types setting (which this engine honours).
    (
        "TestUploads_MembersCannotStoreActiveContent#0.6",
        "set outside the API",
    ),
    // The root user came from the setup endpoint, which a tab does not serve.
    ("TestSetupFlow#0.7", "root user from setup"),
    ("TestSetupFlow#0.20", "root user from setup"),
    // Encrypted (cloak) fields and analytics events are not served yet.
    (
        "TestRealtime_CloakFieldMaskingEndToEnd#0.0",
        "no cloak fields yet",
    ),
    ("TestAppE2EFlow#0.11", "no analytics events yet"),
    // Go's in-process hooks ran custom code; a deployed app carries none.
    (
        "TestRecords_HooksFireForHTTPAndGoWrites#0.0",
        "no custom code hooks",
    ),
];

const NOW: i64 = 1_794_961_964_000;

/// The secret the Go handler tests sign their own tokens with.
const GO_TEST_JWT_SECRET: &[u8] = b"test-secret-key-for-unit-tests-1234";

/// The admin key the Go handler security tests configure.
const GO_SECURITY_ADMIN_KEY: &str = "security-test-admin-key";

/// Keys whose values are timings, not data.
const TIMING_KEYS: &[&str] = &["duration_us"];

#[derive(Default)]
struct Subs {
    /// Go value → Rust value.
    map: BTreeMap<String, String>,
    /// Go tokens that stand for the owner (root users, admin login).
    owner_tokens: Vec<String>,
}

impl Subs {
    fn apply(&self, s: &str) -> String {
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
fn is_dynamic(s: &str) -> bool {
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
    timestamp || jwt || id15 || record_id || storage || hex || field_id || stamp_name
}

/// Compares a Go value with a Rust value, learning substitutions.
fn compare(go: &Value, rs: &Value, subs: &mut Subs, at: &str) -> Result<(), String> {
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
            if g == r {
                return Ok(());
            }
            let expected = subs.apply(g);
            if &expected == r {
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

fn body_bytes(b: &Option<moul_fixtures::Body>) -> Option<Vec<u8>> {
    let b = b.as_ref()?;
    if let Some(j) = &b.json {
        return Some(serde_json::to_vec(j).unwrap());
    }
    if let Some(t) = &b.text {
        return Some(t.clone().into_bytes());
    }
    b.base64.as_ref().map(|s| moul_fixtures::decode_base64(s))
}

fn build_request(ex: &Exchange, subs: &Subs) -> Request {
    let r = &ex.request;
    let mut headers = Vec::new();
    let mut owner = false;
    for (k, vals) in &r.headers {
        for v in vals {
            if k.eq_ignore_ascii_case("Authorization")
                && subs.owner_tokens.iter().any(|t| v.ends_with(t.as_str()))
            {
                owner = true;
                continue;
            }
            headers.push((k.clone(), subs.apply(v)));
        }
    }
    let mut body = body_bytes(&r.body).unwrap_or_default();
    if r.body.as_ref().is_some_and(|b| b.base64.is_none()) {
        body = subs.apply(&String::from_utf8_lossy(&body)).into_bytes();
    }
    let mut req = Request::from_http(
        &r.method,
        &subs.apply(&r.path),
        &subs.apply(&r.query),
        headers,
        &body,
    );
    if owner {
        req.caller = Caller::Owner;
    }
    req
}

fn admin_key(fx: &HttpFixture) -> Option<String> {
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
    // Handler tests that never send the key in a header still configure
    // the one the security tests share.
    Some(GO_SECURITY_ADMIN_KEY.to_owned())
}

fn response_json(r: &Response) -> Option<Value> {
    match &r.body {
        ResponseBody::Json(v) => Some(v.clone()),
        ResponseBody::Bytes { data, content_type } if content_type.contains("json") => {
            serde_json::from_slice(data).ok()
        }
        _ => None,
    }
}

fn expected_json(b: &Option<moul_fixtures::Body>) -> Option<Value> {
    let b = b.as_ref()?;
    if let Some(j) = &b.json {
        return Some(j.clone());
    }
    b.text.as_ref().and_then(|t| serde_json::from_str(t).ok())
}

#[derive(Debug)]
struct Outcome {
    key: String,
    route: String,
    diff: Option<String>,
}

fn replay(fx: &HttpFixture) -> Vec<Outcome> {
    let mut out = Vec::new();
    let key = admin_key(fx);
    for (si, session) in fx.sessions.iter().enumerate() {
        let store = Store::open_in_memory().unwrap();
        if let Some(id) = &session.db_before {
            Fixtures::default()
                .db(id)
                .unwrap()
                .load_into(store.conn())
                .unwrap();
        }
        let config = Config {
            admin_key: key.clone(),
            signing_key: Some(GO_TEST_JWT_SECRET.to_vec()),
            ..Config::default()
        };
        let mut engine = Engine::open(
            store,
            SeededHost::new(si as u64 + 7, NOW),
            MemoryBlobs::new(),
            config,
        )
        .unwrap();
        let mut subs = Subs::default();
        for (ei, ex) in session.exchanges.iter().enumerate() {
            let tag = format!("{}#{si}.{ei}", fx.test);
            if !IN_SCOPE.contains(&ex.route.as_str()) {
                // Tokens of root users stand for the owner from here on.
                if let Some(Value::Object(o)) = expected_json(&ex.response.body)
                    && let Some(Value::String(t)) = o.get("token")
                {
                    subs.owner_tokens.push(t.clone());
                }
                continue;
            }
            let req = build_request(ex, &subs);
            let got = engine.handle(req);
            let mut diff = None;
            if got.status != ex.response.status {
                diff = Some(format!(
                    "status: Go {}, Rust {} ({})",
                    ex.response.status,
                    got.status,
                    got.message()
                ));
            } else if let Some(want) = expected_json(&ex.response.body) {
                match response_json(&got) {
                    Some(have) => {
                        if let Err(d) = compare(&want, &have, &mut subs, "$") {
                            diff = Some(d);
                        }
                    }
                    None => diff = Some("Go answered JSON, Rust did not".into()),
                }
            }
            out.push(Outcome {
                key: tag,
                route: format!("{} {}", ex.request.method, ex.route),
                diff,
            });
        }
    }
    out
}

#[test]
fn go_http_parity() {
    let fx = Fixtures::default();
    let mut passed = 0;
    let mut decided = 0;
    let mut failed = Vec::new();
    let mut by_route: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for path in fx.http_paths().unwrap() {
        let f = fx.http(&path).unwrap();
        if f.outcome != "pass" {
            continue;
        }
        for o in replay(&f) {
            let e = by_route.entry(o.route.clone()).or_default();
            match o.diff {
                None => {
                    passed += 1;
                    e.0 += 1;
                }
                Some(_) if DECIDED.iter().any(|(k, _)| *k == o.key) => decided += 1,
                Some(d) => {
                    e.1 += 1;
                    failed.push(format!("{} {}: {d}", o.key, o.route));
                }
            }
        }
    }
    println!(
        "parity: {passed} match, {decided} decided, {} differ",
        failed.len()
    );
    for (route, (ok, bad)) in &by_route {
        println!("  {ok:4} ok {bad:4} differ  {route}");
    }
    for f in &failed {
        println!("DIFF {f}");
    }
    let limit: usize = std::env::var("MOUL_PARITY_ALLOWED_DIFFS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(
        failed.len() <= limit,
        "{} exchanges differ from the Go engine",
        failed.len()
    );
}

#[test]
fn dynamic_values_are_recognised() {
    assert!(is_dynamic("2026-11-18T00:32:44Z"));
    assert!(is_dynamic("post-2xMlWP1yDVqDSdr"));
    assert!(is_dynamic("o1I5HQ44w6iU5EP"));
    assert!(!is_dynamic("Hello E2E Moul World"));
    let _ = Body::None;
}
