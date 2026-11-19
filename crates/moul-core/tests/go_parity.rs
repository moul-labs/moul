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
//! Exchanges on routes the engine does not serve are skipped: the native
//! server's own (server-sent events, realtime tickets, MCP, the OAuth
//! authorization server, the device flow, logs, metrics), which
//! `crates/moul-server/tests/go_parity.rs` replays through the server. A
//! root user token such a route issued is replayed as the owner. A difference that is a decision, not a bug, is
//! listed in `DECIDED` with its reason.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;

use moul_core::{
    Body, Caller, Config, Engine, MemoryBlobs, Request, Response, ResponseBody, SeededHost,
};
use moul_fixtures::compare::{
    GO_TEST_JWT_SECRET, NOW, Subs, admin_key, body_bytes, compare, expected_json, is_dynamic,
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
    "/api/setup",
    "/api/admin/login",
    "/api/admin/account",
    "/api/admin/password",
    "/api/admin/reload",
    "/api/settings",
    "/api/settings/account",
    "/api/feature-flags",
    "/api/feature-flags/:key",
    "/api/feature-flags/:key/eval",
    "/api/workers",
    "/api/workers/:id",
    "/api/workers/retry",
    "/api/moul/:name/webhooks",
    "/api/moul/:name/email-templates/test",
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
    // Encrypted (cloak) fields and analytics events are not served yet.
    (
        "TestRealtime_CloakFieldMaskingEndToEnd#0.0",
        "no cloak fields yet",
    ),
    ("TestAppE2EFlow#0.11", "no analytics events yet"),
    // Secret settings are never sent back; they read as "********".
    (
        "TestEmailSettingsAndUpdate#0.1",
        "secret settings are masked",
    ),
    ("TestTUIE2E#0.11", "secret settings are masked"),
    // A second flag with a key in use is refused; Go updated the first and
    // answered with an id it never stored.
    (
        "TestAdminSurface_RootOrAdminKeyOnly#0.75",
        "duplicate flag key answers 400",
    ),
    // Jobs read back with their JSON and numbers decoded, as Go answered
    // only when creating one.
    ("TestWorkersAPI#0.4", "jobs read back decoded"),
    ("TestWorkersAPI#0.5", "jobs read back decoded"),
    ("TestWorkersAPI#0.7", "jobs read back decoded"),
    // Go's in-process hooks ran custom code; a deployed app carries none.
    (
        "TestRecords_HooksFireForHTTPAndGoWrites#0.0",
        "no custom code hooks",
    ),
];

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

fn response_json(r: &Response) -> Option<Value> {
    match &r.body {
        ResponseBody::Json(v) => Some(v.clone()),
        ResponseBody::Bytes { data, content_type } if content_type.contains("json") => {
            serde_json::from_slice(data).ok()
        }
        _ => None,
    }
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
    let mut used = std::collections::BTreeSet::new();
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
                Some(_) if DECIDED.iter().any(|(k, _)| *k == o.key) => {
                    decided += 1;
                    used.insert(o.key.clone());
                }
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
    // A decision for an exchange that now matches, or no longer runs, is
    // out of date.
    let stale: Vec<&str> = DECIDED
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| !used.contains(*k))
        .collect();
    assert!(
        stale.is_empty(),
        "DECIDED entries that no longer differ: {stale:?}"
    );
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
