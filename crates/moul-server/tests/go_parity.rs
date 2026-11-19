//! The server's parity suite: replays the HTTP traffic recorded from the Go
//! engine's tests through the whole server (middleware, the server's own
//! routes and the engine) and compares the answers on the routes only the
//! server serves: live streams and tickets, MCP, the OAuth authorization
//! server and device flow, the request log, visits and metrics, and docs.
//! The engine's own routes are compared by `moul-core`'s suite; here they
//! only build the state the server's routes are asked about.
//!
//! Live streams and HTML pages are compared by status. A difference that is
//! a decision is listed in `DECIDED` with its reason.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use moul_core::{Config, Engine, MemoryBlobs, SeededHost};
use moul_fixtures::compare::{
    GO_TEST_JWT_SECRET, NOW, Subs, admin_key, body_bytes, compare, expected_json,
};
use moul_fixtures::{Exchange, Fixtures, HttpFixture};
use moul_server::ServerConfig;
use moul_server::db::{Db, spawn};
use moul_store::Store;
use serde_json::Value;
use tower::ServiceExt;

/// Echo route patterns only the server serves.
const IN_SCOPE: &[&str] = &[
    "/api/realtime/ticket",
    "/api/moul/:name/subscribe",
    "/api/moul/subscribe",
    "/api/mcp*",
    "/mcp*",
    "/.well-known/oauth-protected-resource",
    "/.well-known/oauth-protected-resource/*",
    "/.well-known/oauth-authorization-server",
    "/.well-known/openid-configuration",
    "/api/oauth2/register",
    "/api/oauth2/authorize",
    "/api/oauth2/token",
    "/api/oauth2/device/authorize",
    "/api/oauth2/device/token",
    "/device",
    "/device/verify",
    "/api/requests",
    "/api/visits",
    "/api/system/metrics",
];

/// Routes answered as a stream or a page: compared by status only.
const STATUS_ONLY: &[&str] = &[
    "/api/moul/:name/subscribe",
    "/api/moul/subscribe",
    "/api/oauth2/authorize",
    "/device",
    "/device/verify",
];

/// Differences that are decisions. Key: `<test>#<session>.<exchange>`.
const DECIDED: &[(&str, &str)] = &[
    // Metrics are always sampled here; the Go test ran without a collector.
    (
        "TestAdminSurface_RootOrAdminKeyOnly#0.58",
        "metrics always collected",
    ),
    (
        "TestAdminSurface_RootOrAdminKeyOnly#0.59",
        "metrics always collected",
    ),
    // The Go test named the app in its own code, which a recording cannot carry.
    (
        "TestConfiguredAppNameInOAuthAndMCP#0.1",
        "app name set in Go code",
    ),
    // A client must register before it asks for consent; Go registered any
    // client_id it was shown, and added any redirect_uri to it.
    (
        "TestConfiguredAppNameInOAuthAndMCP#0.2",
        "unregistered clients refused",
    ),
    // RFC 7591 asks for client_secret_expires_at whenever a secret is issued.
    ("TestGeminiMCPOAuthFlow#0.6", "client_secret_expires_at"),
    (
        "TestOAuthAuthorizeWithRootUserAndRefreshToken#0.0",
        "client_secret_expires_at",
    ),
    // PKCE is S256 only.
    ("TestGeminiMCPOAuthFlow#0.4", "S256 only"),
    ("TestGeminiMCPOAuthFlow#0.5", "S256 only"),
    // MCP reports the build's version, not "dev".
    ("TestGeminiMCPOAuthFlow#0.10", "real version"),
    ("TestGeminiMCPOAuthFlow#0.11", "real version"),
    ("TestMCPRouterEndpoints#0.0", "real version"),
    // A refresh token is used once and replaced; Go handed back the same
    // one, which the test then used again.
    (
        "TestOAuthAuthorizeWithRootUserAndRefreshToken#0.3",
        "refresh tokens rotate",
    ),
    // The recording lists exchanges in the order they finished: long-lived
    // streams come after requests that started later, so a replay in that
    // order meets the limits and single-use tickets the other way round.
    (
        "TestRealtimeSSE_ConcurrentConnectionLimit#0.1",
        "streams recorded as they ended",
    ),
    (
        "TestRealtimeSSE_ConcurrentConnectionLimit#0.3",
        "streams recorded as they ended",
    ),
    (
        "TestRealtimeSSE_ConcurrentConnectionLimit#0.4",
        "streams recorded as they ended",
    ),
    (
        "TestRealtimeSubscribeWithSingleUseTicket#0.1",
        "streams recorded as they ended",
    ),
    (
        "TestRealtimeSubscribeWithSingleUseTicket#0.2",
        "streams recorded as they ended",
    ),
    // Visits are read here but not counted yet.
    ("TestTUIE2E#0.22", "visits not counted yet"),
];

#[derive(Debug)]
struct Outcome {
    key: String,
    route: String,
    diff: Option<String>,
}

fn request(ex: &Exchange, subs: &Subs) -> axum::http::Request<Body> {
    let r = &ex.request;
    let mut target = subs.apply(&r.path);
    if !r.query.is_empty() {
        target.push('?');
        target.push_str(&subs.apply(&r.query));
    }
    let mut b = axum::http::Request::builder()
        .method(r.method.as_str())
        .uri(target);
    for (k, vals) in &r.headers {
        if k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        for v in vals {
            b = b.header(k.as_str(), subs.apply(v));
        }
    }
    let mut body = body_bytes(&r.body).unwrap_or_default();
    if r.body.as_ref().is_some_and(|b| b.base64.is_none()) {
        body = subs.apply(&String::from_utf8_lossy(&body)).into_bytes();
    }
    let mut req = b.body(Body::from(body)).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 1234))));
    req
}

async fn replay(fx: &HttpFixture) -> Vec<Outcome> {
    let mut out = Vec::new();
    let key = admin_key(fx);
    for (si, session) in fx.sessions.iter().enumerate() {
        let db_before = session.db_before.clone();
        let k = key.clone();
        let seed = si as u64 + 7;
        let (db, thread) = spawn("parity", move || {
            let store = Store::open_in_memory().map_err(|e| e.to_string())?;
            if let Some(id) = &db_before {
                Fixtures::default()
                    .db(id)
                    .map_err(|e| e.to_string())?
                    .load_into(store.conn())
                    .map_err(|e| e.to_string())?;
            }
            let config = Config {
                admin_key: k,
                signing_key: Some(GO_TEST_JWT_SECRET.to_vec()),
                public_url: "http://example.com".into(),
                ..Config::default()
            };
            let engine = Engine::open(
                store,
                SeededHost::new(seed, NOW),
                MemoryBlobs::new(),
                config,
            )
            .map_err(|e| e.to_string())?;
            Ok((Db::new(engine), Vec::new()))
        })
        .unwrap();
        let state = moul_server::server::state(
            ServerConfig {
                allow_private_calls: true,
                ..ServerConfig::default()
            },
            db.clone(),
            key.clone().unwrap_or_default(),
        )
        .await;
        let app = moul_server::http::router(state);
        let mut subs = Subs::default();
        // Streams stay open until the session ends, as in the Go test.
        let mut open = Vec::new();
        for (ei, ex) in session.exchanges.iter().enumerate() {
            let tag = format!("{}#{si}.{ei}", fx.test);
            let resp = app.clone().oneshot(request(ex, &subs)).await.unwrap();
            let status = resp.status().as_u16();
            let streaming = resp
                .headers()
                .get("content-type")
                .is_some_and(|v| v.to_str().unwrap_or("").starts_with("text/event-stream"));
            let in_scope = IN_SCOPE.contains(&ex.route.as_str());
            // A redirect carries codes and states in its address.
            if let (Some(go), Some(rs)) = (
                ex.response.headers.get("Location").and_then(|v| v.first()),
                resp.headers().get("location").and_then(|v| v.to_str().ok()),
            ) {
                let params =
                    |u: &str| moul_core::api::parse_query(u.split_once('?').map_or("", |(_, q)| q));
                let rs_params = params(rs);
                for (k, gv) in params(go) {
                    if let Some((_, rv)) = rs_params.iter().find(|(rk, _)| *rk == k)
                        && gv != *rv
                        && moul_fixtures::compare::is_dynamic(&gv)
                    {
                        subs.map.insert(gv, rv.clone());
                    }
                }
            }
            let body = if streaming {
                open.push(resp);
                Vec::new()
            } else {
                axum::body::to_bytes(resp.into_body(), 64 << 20)
                    .await
                    .unwrap()
                    .to_vec()
            };
            let got: Option<Value> = serde_json::from_slice(&body).ok();
            if !in_scope {
                // Learn the engine's ids and tokens for later requests.
                if let (Some(want), Some(got)) = (expected_json(&ex.response.body), &got) {
                    let _ = compare(&want, got, &mut subs, "$");
                }
                continue;
            }
            let mut diff = None;
            if status != ex.response.status {
                let msg = got
                    .as_ref()
                    .and_then(|g| {
                        g["message"]
                            .as_str()
                            .or(g["error"].as_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| String::from_utf8_lossy(&body).chars().take(80).collect());
                diff = Some(format!(
                    "status: Go {}, Rust {status} ({msg})",
                    ex.response.status
                ));
            } else if !STATUS_ONLY.contains(&ex.route.as_str())
                && let Some(want) = expected_json(&ex.response.body)
            {
                match &got {
                    Some(g) => {
                        if let Err(e) = compare(&want, g, &mut subs, "$") {
                            diff = Some(e);
                        }
                    }
                    None => diff = Some("Rust answered no JSON".into()),
                }
            }
            out.push(Outcome {
                key: tag,
                route: format!("{} {}", ex.request.method, ex.route),
                diff,
            });
        }
        drop(open);
        db.close();
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
    }
    out
}

#[tokio::test]
async fn go_server_parity() {
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
        for o in replay(&f).await {
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
        "server parity: {passed} match, {decided} decided, {} differ",
        failed.len()
    );
    for (route, (ok, bad)) in &by_route {
        println!("  {ok:4} ok {bad:4} differ  {route}");
    }
    for f in &failed {
        println!("DIFF {f}");
    }
    let stale: Vec<&str> = DECIDED
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| !used.contains(*k))
        .collect();
    assert!(
        stale.is_empty(),
        "DECIDED entries that no longer differ: {stale:?}"
    );
    assert!(
        failed.is_empty(),
        "{} exchanges differ from the Go engine",
        failed.len()
    );
}
