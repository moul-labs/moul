//! A real server on a loopback port, driven over HTTP.

#![cfg(not(target_arch = "wasm32"))]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use moul_server::{Running, ServerConfig, start};
use serde_json::{Value, json};

const KEY: &str = "test-admin-key";

fn temp(name: &str) -> PathBuf {
    let mut b = [0u8; 6];
    getrandom::fill(&mut b).unwrap();
    let p = std::env::temp_dir().join(format!("moul-server-{name}-{}", moul_core::ids::hex(&b)));
    let _ = std::fs::remove_dir_all(&p);
    p
}

async fn server(name: &str, tweak: impl FnOnce(&mut ServerConfig)) -> (Running, String, PathBuf) {
    let dir = temp(name);
    let mut c = ServerConfig {
        dir: dir.join("db"),
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        admin_key: Some(KEY.into()),
        allow_private_calls: true,
        ..ServerConfig::default()
    };
    tweak(&mut c);
    let r = start(c).await.unwrap();
    let base = format!("http://{}", r.addr);
    (r, base, dir)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn send(req: reqwest::RequestBuilder) -> (u16, reqwest::header::HeaderMap, Value) {
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    let bytes = r.bytes().await.unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn json_post(c: &reqwest::Client, url: String, body: Value) -> reqwest::RequestBuilder {
    c.post(url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
}

async fn notes(c: &reqwest::Client, base: &str, extra: Value) {
    let mut def = json!({
        "name": "notes", "type": "base",
        "fields": [{"name": "title", "type": "text"}],
        "rules": {"listRule": "", "viewRule": "", "createRule": "", "updateRule": "", "deleteRule": ""},
    });
    if let (Some(d), Some(e)) = (def.as_object_mut(), extra.as_object()) {
        d.extend(e.clone());
    }
    let (s, _, b) =
        send(json_post(c, format!("{base}/api/moul"), def).header("X-Admin-Key", KEY)).await;
    assert_eq!(s, 201, "{b}");
}

#[tokio::test]
async fn records_live_events_and_tickets() {
    let (srv, base, dir) = server("live", |_| {}).await;
    let c = client();
    notes(&c, &base, json!({})).await;

    // A live stream, then a record made: the stream sees it.
    let mut stream = c
        .get(format!("{base}/api/moul/notes/subscribe"))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    let first = String::from_utf8(stream.chunk().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(
        first.starts_with("event: connected\ndata: {\"client_id\":"),
        "{first}"
    );
    let (s, h, rec) = send(json_post(
        &c,
        format!("{base}/api/moul/notes/records"),
        json!({"title": "hi"}),
    ))
    .await;
    assert_eq!(s, 201);
    assert!(h.contains_key("moul-request-id"));
    let mut seen = String::new();
    while !seen.contains("\n\n") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seen.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert!(
        seen.starts_with("event: create\ndata: {\"action\":\"create\""),
        "{seen}"
    );
    assert!(seen.contains(rec["id"].as_str().unwrap()));
    drop(stream);

    // Tickets need a signed-in caller, work once, and carry their creator.
    let (s, _, b) = send(c.post(format!("{base}/api/realtime/ticket"))).await;
    assert_eq!(
        (s, b["message"].as_str()),
        (
            401,
            Some("Authentication required to request a realtime ticket")
        )
    );
    let (s, _, t) = send(
        c.post(format!("{base}/api/realtime/ticket"))
            .header("X-Admin-Key", KEY),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(t["expiresIn"], 30);
    let ticket = t["ticket"].as_str().unwrap();
    let ok = c
        .get(format!("{base}/api/moul/subscribe?ticket={ticket}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok.status(),
        200,
        "the owner's ticket opens the stream of every collection"
    );
    drop(ok);
    let again = c
        .get(format!("{base}/api/moul/notes/subscribe?ticket={ticket}"))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 401);

    // Credentials in the query string are refused outright.
    let (s, _, _) = send(c.get(format!("{base}/api/moul/notes/records?token=abc"))).await;
    assert_eq!(s, 400);
    let (s, _, b) = send(c.get(format!("{base}/api/moul/nope/subscribe"))).await;
    assert_eq!(
        (s, b["message"].as_str()),
        (404, Some("Moul collection \"nope\" not found"))
    );

    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn cors_challenges_and_the_request_log() {
    let (srv, base, dir) = server("cors", |c| {
        c.cors_origins = vec!["https://app.example".into()]
    })
    .await;
    let c = client();
    let pre = c
        .request(
            reqwest::Method::OPTIONS,
            format!("{base}/api/moul/notes/records"),
        )
        .header("Origin", "https://app.example")
        .header("Access-Control-Request-Method", "PUT")
        .send()
        .await
        .unwrap();
    assert_eq!(pre.status(), 204);
    assert_eq!(
        pre.headers()["access-control-allow-origin"],
        "https://app.example"
    );
    assert!(
        pre.headers()["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .contains("If-Match")
    );
    let other = c
        .get(format!("{base}/api/health"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert!(!other.headers().contains_key("access-control-allow-origin"));

    let (s, h, _) = send(c.get(format!("{base}/api/requests"))).await;
    assert_eq!(s, 401);
    let challenge = h["www-authenticate"].to_str().unwrap();
    assert!(
        challenge.contains("resource_metadata=\"http://127.0.0.1:"),
        "{challenge}"
    );

    // Requests are logged in batches, every few seconds.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let (s, _, log) = send(
        c.get(format!("{base}/api/requests"))
            .header("X-Admin-Key", KEY),
    )
    .await;
    assert_eq!(s, 200);
    assert!(
        log["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "/api/health"),
        "{log}"
    );
    assert!(
        log["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["path"] != "/api/requests")
    );
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn rate_limits_count_per_address() {
    let (srv, base, dir) = server("limits", |_| {}).await;
    let c = client();
    let rules = r#"[{"label":"/api/health","maxRequests":2,"interval":60,"targetedUsers":"all"}]"#;
    let (s, _, _) = send(
        c.patch(format!("{base}/api/settings"))
            .header("X-Admin-Key", KEY)
            .header("Content-Type", "application/json")
            .body(json!({"rate_limiting_rules": rules}).to_string()),
    )
    .await;
    assert_eq!(s, 200);
    let mut statuses = Vec::new();
    for _ in 0..3 {
        statuses.push(
            c.get(format!("{base}/api/health"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
        );
    }
    assert_eq!(statuses, [200, 200, 429]);
    // The admin key is never limited.
    let s = c
        .get(format!("{base}/api/health"))
        .header("X-Admin-Key", KEY)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(s, 200);
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// A server that records what it receives, for webhooks.
async fn receiver() -> (
    String,
    tokio::sync::mpsc::Receiver<(axum::http::HeaderMap, String)>,
) {
    use axum::routing::post;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let app = axum::Router::new().route(
        "/hook",
        post(move |headers: axum::http::HeaderMap, body: String| {
            let tx = tx.clone();
            async move {
                let _ = tx.send((headers, body)).await;
                "ok"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (url, rx)
}

#[tokio::test]
async fn webhooks_are_delivered_signed_and_tested() {
    let (srv, base, dir) = server("hooks", |_| {}).await;
    let c = client();
    notes(&c, &base, json!({})).await;
    let (url, mut got) = receiver().await;
    let (s, _, hook) = send(
        json_post(
            &c,
            format!("{base}/api/moul/notes/webhooks"),
            json!({"url": url, "events": ["create"], "secret": "shh"}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    assert_eq!(s, 201, "{hook}");
    send(json_post(
        &c,
        format!("{base}/api/moul/notes/records"),
        json!({"title": "hooked"}),
    ))
    .await;
    let (headers, body) = tokio::time::timeout(Duration::from_secs(10), got.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(headers["x-moul-event"], "create:after");
    let sig = headers["x-moul-signature"].to_str().unwrap();
    assert_eq!(
        sig,
        moul_server::webhooks::signature("shh", body.as_bytes())
    );
    let payload: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(payload["record"]["title"], "hooked");

    let (s, _, t) = send(
        c.post(format!(
            "{base}/api/moul/notes/webhooks/{}/test",
            hook["id"].as_str().unwrap()
        ))
        .header("X-Admin-Key", KEY),
    )
    .await;
    assert_eq!((s, t["success"].as_bool()), (200, Some(true)), "{t}");
    let (headers, _) = tokio::time::timeout(Duration::from_secs(5), got.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(headers["x-moul-event"], "ping");
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn webhooks_never_reach_private_addresses_by_default() {
    let (srv, base, dir) = server("private", |c| c.allow_private_calls = false).await;
    let c = client();
    notes(&c, &base, json!({})).await;
    let (s, _, t) = send(
        json_post(
            &c,
            format!("{base}/api/moul/notes/webhooks/test"),
            json!({"url": "http://169.254.169.254/latest"}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(t["success"], false);
    assert!(
        t["error"]
            .as_str()
            .unwrap()
            .contains("not a public address"),
        "{t}"
    );
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn mcp_clients_register_sign_in_and_call_tools() {
    let (srv, base, dir) = server("mcp", |_| {}).await;
    let c = client();
    notes(&c, &base, json!({})).await;
    let (s, _, _) = send(
        json_post(
            &c,
            format!("{base}/api/setup"),
            json!({"username": "root", "email": "root@x.co", "password": "supersecret1A"}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    assert_eq!(s, 200);

    // An unregistered client is turned away.
    let (s, _, _) = send(c.get(format!(
        "{base}/api/oauth2/authorize?client_id=nope&redirect_uri=http://localhost:1/cb"
    )))
    .await;
    assert_eq!(s, 400);
    let (s, _, reg) = send(json_post(
        &c,
        format!("{base}/api/oauth2/register"),
        json!({"client_name": "Agent", "redirect_uris": ["http://localhost:9/cb"], "token_endpoint_auth_method": "none"}),
    ))
    .await;
    assert_eq!(s, 201);
    let client_id = reg["client_id"].as_str().unwrap();
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = moul_core::oauth::challenge(verifier);
    let page = c
        .get(format!(
            "{base}/api/oauth2/authorize?client_id={client_id}&redirect_uri=http://localhost:9/cb&response_type=code&code_challenge={challenge}&code_challenge_method=S256&state=xyz"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("Allow Agent"));
    // The admin key is not a password here.
    let no_follow = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let form = |identity: &str, password: &str| {
        format!(
            "client_id={client_id}&redirect_uri=http%3A%2F%2Flocalhost%3A9%2Fcb&response_type=code&code_challenge={challenge}&code_challenge_method=S256&state=xyz&identity={identity}&password={password}"
        )
    };
    let refused = no_follow
        .post(format!("{base}/api/oauth2/authorize"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form("root", KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    let ok = no_follow
        .post(format!("{base}/api/oauth2/authorize"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form("root", "supersecret1A"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 302);
    let location = ok.headers()["location"].to_str().unwrap().to_owned();
    assert!(
        location.starts_with("http://localhost:9/cb?code=") && location.ends_with("&state=xyz"),
        "{location}"
    );
    let code = location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let token_form = format!(
        "grant_type=authorization_code&code={code}&client_id={client_id}&redirect_uri=http%3A%2F%2Flocalhost%3A9%2Fcb&code_verifier={verifier}"
    );
    let (s, _, tok) = send(
        c.post(format!("{base}/api/oauth2/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(token_form.clone()),
    )
    .await;
    assert_eq!(s, 200, "{tok}");
    let access = tok["access_token"].as_str().unwrap().to_owned();
    let refresh = tok["refresh_token"].as_str().unwrap().to_owned();
    // A code works once.
    let (s, _, _) = send(
        c.post(format!("{base}/api/oauth2/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(token_form),
    )
    .await;
    assert_eq!(s, 400);
    // A refresh token works once and is replaced.
    let rf = format!("grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}");
    let (s, _, next) = send(
        c.post(format!("{base}/api/oauth2/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(rf.clone()),
    )
    .await;
    assert_eq!(s, 200);
    assert_ne!(next["refresh_token"], tok["refresh_token"]);
    let (s, _, _) = send(
        c.post(format!("{base}/api/oauth2/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(rf),
    )
    .await;
    assert_eq!(s, 400);

    // MCP with the token.
    let init = c
        .post(format!("{base}/api/mcp"))
        .bearer_auth(&access)
        .header("Content-Type", "application/json")
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}}).to_string())
        .send()
        .await
        .unwrap();
    let session = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body: Value = init.json_body().await;
    assert_eq!(body["result"]["protocolVersion"], "2024-11-05");
    let call = |name: &str, args: Value| {
        c.post(format!("{base}/api/mcp"))
            .bearer_auth(&access)
            .header("Mcp-Session-Id", &session)
            .header("Content-Type", "application/json")
            .body(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string())
    };
    let made: Value = call(
        "moul_create_record",
        json!({"collection": "notes", "data_json": "{\"title\":\"from mcp\"}"}),
    )
    .send()
    .await
    .unwrap()
    .json_body()
    .await;
    assert!(made["result"]["isError"].is_null(), "{made}");
    let q: Value = call("moul_query", json!({"sql": "SELECT title FROM notes"}))
        .send()
        .await
        .unwrap()
        .json_body()
        .await;
    assert!(
        q["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("from mcp")
    );
    let secret: Value = call(
        "moul_query",
        json!({"sql": "SELECT * FROM writer._rootUsers"}),
    )
    .send()
    .await
    .unwrap()
    .json_body()
    .await;
    assert_eq!(secret["result"]["isError"], true);
    let no_session = c
        .post(format!("{base}/api/mcp"))
        .bearer_auth(&access)
        .header("Content-Type", "application/json")
        .body(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(no_session.status(), 404);
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

trait JsonBody {
    async fn json_body(self) -> Value;
}

impl JsonBody for reqwest::Response {
    async fn json_body(self) -> Value {
        serde_json::from_slice(&self.bytes().await.unwrap()).unwrap_or(Value::Null)
    }
}

#[tokio::test]
async fn devices_sign_in_through_the_device_flow_once() {
    let (srv, base, dir) = server("device", |_| {}).await;
    let c = client();
    send(
        json_post(
            &c,
            format!("{base}/api/setup"),
            json!({"username": "root", "email": "root@x.co", "password": "supersecret1A"}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    let (_, _, d) = send(json_post(
        &c,
        format!("{base}/api/oauth2/device/authorize"),
        json!({"client_id": "cli"}),
    ))
    .await;
    let device_code = d["device_code"].as_str().unwrap().to_owned();
    let poll = || {
        c.post(format!("{base}/api/oauth2/device/token"))
            .header("Content-Type", "application/json")
            .body(json!({"grant_type": "urn:ietf:params:oauth:grant-type:device_code", "device_code": device_code}).to_string())
    };
    let (s, _, p) = send(poll()).await;
    assert_eq!(
        (s, p["error"].as_str()),
        (400, Some("authorization_pending"))
    );
    let (s, _, p) = send(poll()).await;
    assert_eq!((s, p["error"].as_str()), (400, Some("slow_down")));
    let (s, _, v) = send(
        json_post(
            &c,
            format!("{base}/device/verify"),
            json!({"user_code": d["user_code"], "identity": "root", "password": "supersecret1A"}),
        )
        .header("Accept", "application/json"),
    )
    .await;
    assert_eq!((s, v["success"].as_bool()), (200, Some(true)), "{v}");
    let (s, _, t) = send(poll()).await;
    assert_eq!(s, 200);
    assert!(t["access_token"].is_string());
    let (s, _, p) = send(poll()).await;
    assert_eq!(
        (s, p["error"].as_str()),
        (400, Some("expired_token")),
        "a token is handed out once"
    );
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn replicates_to_a_folder_that_restores() {
    let (srv, base, dir) = server("replica", |c| {
        c.replica = Some(
            c.dir
                .parent()
                .unwrap()
                .join("replica")
                .display()
                .to_string(),
        );
    })
    .await;
    let c = client();
    let (s, _, b) = send(
        json_post(
            &c,
            format!("{base}/api/setup"),
            json!({"username": "root", "email": "root@x.co", "password": "supersecret1A"}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    assert!(s < 300, "{s} {b}");
    notes(&c, &base, json!({})).await;
    for i in 0..3 {
        send(json_post(
            &c,
            format!("{base}/api/moul/notes/records"),
            json!({"title": format!("n{i}")}),
        ))
        .await;
    }
    // Stopping pushes what is left, of both stores.
    srv.stop().await;
    let replica = moul_sync::DirReplica::new(dir.join("replica"));
    let restore = |r: &dyn moul_sync::Replica| {
        moul_sync::block_on(moul_sync::restore::restore_db(r, None))
            .unwrap()
            .1
    };
    let shared = restore(&replica);
    let out = dir.join("restored.db");
    std::fs::write(&out, &shared).unwrap();
    let conn = rusqlite::Connection::open(&out).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 3);
    let roots: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = '_rootUsers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(roots, 0, "secrets stay out of the shared store");

    // The writer-only store sits beside it, and restores on its own.
    assert!(
        dir.join("replica/moul/writer-only/moul/head.json")
            .is_file()
    );
    let writer_only = restore(&moul_sync::Prefixed::writer_only(&replica));
    let out = dir.join("restored-writer.db");
    std::fs::write(&out, &writer_only).unwrap();
    let conn = rusqlite::Connection::open(&out).unwrap();
    let root: String = conn
        .query_row("SELECT username FROM _rootUsers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(root, "root");

    // A server on the two restored files is the same server: its root user
    // signs in with the same password.
    let (srv2, base2, dir2) = server("replica-restored", |c| {
        std::fs::create_dir_all(&c.dir).unwrap();
        std::fs::write(c.dir.join("main.db"), &shared).unwrap();
        std::fs::write(c.dir.join("writer.db"), &writer_only).unwrap();
    })
    .await;
    let login = |password: &str| {
        json_post(
            &c,
            format!("{base2}/api/admin/login"),
            json!({"identity": "root", "password": password}),
        )
        .header("X-Admin-Key", KEY)
    };
    let (s, _, b) = send(login("wrong password")).await;
    assert_eq!(s, 400, "{b}");
    let (s, _, b) = send(login("supersecret1A")).await;
    assert_eq!(s, 200, "{b}");
    assert!(b["token"].is_string(), "{b}");
    let (_, _, list) = send(c.get(format!("{base2}/api/moul/notes/records"))).await;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(3), "{list}");
    srv2.stop().await;
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
}

#[tokio::test]
async fn a_new_folder_fills_from_a_database_file_and_refuses_twice() {
    // Make a database, export it, start another from the file.
    let (srv, base, dir) = server("from", |_| {}).await;
    let c = client();
    notes(&c, &base, json!({})).await;
    send(json_post(
        &c,
        format!("{base}/api/moul/notes/records"),
        json!({"title": "carried"}),
    ))
    .await;
    srv.stop().await;
    let exported = {
        let (db, thread, _) = moul_server::server::open_db(&ServerConfig {
            dir: dir.join("db"),
            ..ServerConfig::default()
        })
        .unwrap();
        let bytes = db
            .run(|db| db.engine.store().export().unwrap())
            .await
            .unwrap();
        db.close();
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .unwrap();
        bytes
    };
    std::fs::write(dir.join("export.db"), exported).unwrap();
    let (srv2, base2, dir2) = server("from2", |c| c.import = Some(dir.join("export.db"))).await;
    let (_, _, list) = send(c.get(format!("{base2}/api/moul/notes/records"))).await;
    assert_eq!(list["items"][0]["title"], "carried");
    let again = ServerConfig {
        dir: dir2.join("db"),
        import: Some(dir.join("export.db")),
        ..ServerConfig::default()
    };
    srv2.stop().await;
    let refused = moul_server::server::open_db(&again);
    assert!(refused.err().unwrap().contains("already holds a database"));
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
}

#[tokio::test]
async fn root_password_guesses_count_as_sign_ins() {
    let (srv, base, dir) = server("guesses", |_| {}).await;
    let c = client();
    send(
        json_post(
            &c,
            format!("{base}/api/setup"),
            json!({"username": "root", "email": "root@x.co", "password": "supersecret1A"}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    let mut statuses = Vec::new();
    for _ in 0..12 {
        let s = c
            .post(format!("{base}/device/verify"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(
                json!({"user_code": "AAAA-BBBB", "identity": "root", "password": "wrong"})
                    .to_string(),
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        statuses.push(s);
    }
    assert!(
        statuses.contains(&429),
        "the *:auth rule limits them: {statuses:?}"
    );

    // Registration is bounded.
    let many: Vec<String> = (0..11).map(|i| format!("http://localhost:9/{i}")).collect();
    let (s, _, _) = send(
        json_post(
            &c,
            format!("{base}/api/oauth2/register"),
            json!({"redirect_uris": many}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    assert!(s == 400 || s == 429, "{s}");
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_password_change_ends_mcp_refresh_tokens() {
    let (srv, base, dir) = server("rotate", |_| {}).await;
    let c = client();
    send(
        json_post(
            &c,
            format!("{base}/api/setup"),
            json!({"username": "root", "email": "root@x.co", "password": "supersecret1A"}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    let (_, _, reg) = send(json_post(
        &c,
        format!("{base}/api/oauth2/register"),
        json!({"redirect_uris": ["http://localhost:9/cb"], "token_endpoint_auth_method": "none"}),
    ))
    .await;
    let id = reg["client_id"].as_str().unwrap();
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let ch = moul_core::oauth::challenge(verifier);
    let no_follow = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let ok = no_follow
        .post(format!("{base}/api/oauth2/authorize"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("client_id={id}&redirect_uri=http%3A%2F%2Flocalhost%3A9%2Fcb&code_challenge={ch}&code_challenge_method=S256&identity=root&password=supersecret1A"))
        .send()
        .await
        .unwrap();
    let location = ok.headers()["location"].to_str().unwrap().to_owned();
    let code = location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let (_, _, tok) = send(
        c.post(format!("{base}/api/oauth2/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "grant_type=authorization_code&code={code}&client_id={id}&code_verifier={verifier}"
            )),
    )
    .await;
    let refresh = tok["refresh_token"].as_str().unwrap();
    let (s, _, _) = send(
        json_post(
            &c,
            format!("{base}/api/admin/password"),
            json!({"currentPassword": "supersecret1A", "password": "Another1Secret"}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _, _) = send(
        c.post(format!("{base}/api/oauth2/token"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "grant_type=refresh_token&refresh_token={refresh}&client_id={id}"
            )),
    )
    .await;
    assert_eq!(s, 400, "the agent must be approved again");
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_read_only_server_never_claims_its_replica() {
    let (srv, base, dir) = server("readonly", |c| {
        c.follower = true;
        c.replica = Some(
            c.dir
                .parent()
                .unwrap()
                .join("replica")
                .display()
                .to_string(),
        );
    })
    .await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        !dir.join("replica")
            .join("moul")
            .join("writer.json")
            .exists()
    );
    assert!(
        !dir.join("replica")
            .join("moul")
            .join("writer-only")
            .exists(),
        "a server that does not write never pushes its writer-only store"
    );
    let (s, _, _) = send(
        json_post(
            &client(),
            format!("{base}/api/moul"),
            json!({"name": "x", "type": "base", "fields": []}),
        )
        .header("X-Admin-Key", KEY),
    )
    .await;
    assert_eq!(s, 409);
    srv.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}
