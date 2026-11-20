//! A real hub on a loopback port: sign-in, devices, databases, the replica
//! calls, and two devices syncing through it.

#![cfg(not(target_arch = "wasm32"))]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use moul_hub::api::Signup;
use moul_hub::{HubConfig, RunningHub, start};
use moul_store::Store;
use moul_sync::{HubConfig as Remote, HubReplica, Options, StoreLocal, Sync, block_on};
use moul_vfs::{Capture, MemoryStorage, Volume};
use serde_json::{Value, json};

fn temp(name: &str) -> PathBuf {
    let mut b = [0u8; 6];
    getrandom::fill(&mut b).unwrap();
    let p = std::env::temp_dir().join(format!("moul-hub-{name}-{}", moul_core::ids::hex(&b)));
    let _ = std::fs::remove_dir_all(&p);
    p
}

async fn hub(name: &str, signup: Signup, max_bytes: i64) -> (RunningHub, String, PathBuf) {
    hub_on(name, signup, max_bytes, None).await
}

async fn hub_on(
    name: &str,
    signup: Signup,
    max_bytes: i64,
    store: Option<moul_hub::store::Store>,
) -> (RunningHub, String, PathBuf) {
    let dir = temp(name);
    let h = start(HubConfig {
        dir: dir.clone(),
        store,
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        signup,
        max_bytes,
        admin_key: Some("hub-admin".into()),
        ..HubConfig::default()
    })
    .await
    .unwrap();
    let base = format!("http://{}", h.addr);
    (h, base, dir)
}

async fn call(
    method: reqwest::Method,
    url: String,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut r = reqwest::Client::new().request(method, url);
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    if let Some(b) = body {
        r = r
            .header("Content-Type", "application/json")
            .body(b.to_string());
    }
    let resp = r.send().await.unwrap();
    let s = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap();
    (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The newest code mailed to `email`, from the hub's inbox (mail is off).
async fn code_for(h: &RunningHub, email: &str) -> Option<String> {
    let to = email.to_owned();
    let inbox = h
        .state
        .db
        .run(move |db| {
            db.engine
                .handle(
                    moul_core::Request::get("/api/inbox")
                        .query("to", &to)
                        .owner(),
                )
                .json_body()
                .clone()
        })
        .await
        .unwrap();
    let body = inbox["items"][0]["body"].as_str()?.to_owned();
    body.split(|c: char| !c.is_ascii_digit())
        .find(|w| w.len() == 6)
        .map(str::to_owned)
}

async fn sign_in(h: &RunningHub, base: &str, email: &str) -> String {
    let (s, b) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/signin"),
        None,
        Some(json!({"email": email})),
    )
    .await;
    assert_eq!((s, b["sent"].as_bool()), (200, Some(true)));
    let code = code_for(h, email).await.expect("a code was mailed");
    let (s, v) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/verify"),
        None,
        Some(json!({"email": email, "code": code, "label": "Laptop"})),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["account"]["email"], email);
    v["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn sign_up_follows_the_hub_setting() {
    let (h, base, dir) = hub("signup", Signup::Invite, 1 << 30).await;
    // Without an invite nothing is sent, and the answer does not say so.
    let (s, b) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/signin"),
        None,
        Some(json!({"email": "a@b.co"})),
    )
    .await;
    assert_eq!((s, b["sent"].as_bool()), (200, Some(true)));
    assert_eq!(code_for(&h, "a@b.co").await, None);
    let (s, inv) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/admin/invites"),
        None,
        Some(json!({"uses": 1})),
    )
    .await;
    assert_eq!(s, 401, "invites need the admin key");
    let r = reqwest::Client::new()
        .post(format!("{base}/api/hub/admin/invites"))
        .header("X-Admin-Key", "hub-admin")
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    let inv: Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap_or(inv);
    let code = inv["code"].as_str().unwrap();
    call(
        reqwest::Method::POST,
        format!("{base}/api/hub/signin"),
        None,
        Some(json!({"email": "a@b.co", "invite": code})),
    )
    .await;
    assert!(code_for(&h, "a@b.co").await.is_some());
    // The admin sets mail up through the directory's settings; nothing
    // else of the directory's API is served.
    let c = reqwest::Client::new();
    let patch = |key: Option<&'static str>| {
        let mut r = c
            .patch(format!("{base}/api/settings"))
            .header("Content-Type", "application/json")
            .body(json!({"email_provider": "resend", "email_api_key": "re_secret"}).to_string());
        if let Some(k) = key {
            r = r.header("X-Admin-Key", k);
        }
        r.send()
    };
    assert_eq!(patch(None).await.unwrap().status(), 401);
    let set: Value = serde_json::from_slice(
        &patch(Some("hub-admin"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            set["email_provider"].as_str(),
            set["email_api_key"].as_str()
        ),
        (Some("resend"), Some("********"))
    );
    let hidden = c
        .get(format!("{base}/api/moul/accounts/records"))
        .header("X-Admin-Key", "hub-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(hidden.status(), 404);

    // The invite was for one sign-up.
    call(
        reqwest::Method::POST,
        format!("{base}/api/hub/signin"),
        None,
        Some(json!({"email": "c@d.co", "invite": code})),
    )
    .await;
    assert_eq!(code_for(&h, "c@d.co").await, None);
    h.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn devices_databases_and_join_tickets() {
    let (h, base, dir) = hub("devices", Signup::Open, 1 << 30).await;
    let token = sign_in(&h, &base, "dev@x.co").await;
    let (s, me) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/me"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(me["device"]["label"], "Laptop");
    assert_eq!(me["device"]["database"], Value::Null);

    for name in ["notes", "photos"] {
        let (s, _) = call(
            reqwest::Method::POST,
            format!("{base}/api/hub/databases"),
            Some(&token),
            Some(json!({"name": name})),
        )
        .await;
        assert_eq!(s, 201);
    }
    let (s, _) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/databases"),
        Some(&token),
        Some(json!({"name": "notes"})),
    )
    .await;
    assert_eq!(s, 409);
    let (s, _) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/databases"),
        Some(&token),
        Some(json!({"name": "Bad Name"})),
    )
    .await;
    assert_eq!(s, 400);

    // A ticket makes a device that reaches one database only.
    let (s, t) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/databases/notes/joins"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(s, 201);
    let (s, j) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/join"),
        None,
        Some(json!({"ticket": t["ticket"], "label": "Phone"})),
    )
    .await;
    assert_eq!((s, j["database"].as_str()), (200, Some("notes")));
    let phone = j["token"].as_str().unwrap().to_owned();
    let (s, _) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/join"),
        None,
        Some(json!({"ticket": t["ticket"]})),
    )
    .await;
    assert_eq!(s, 400, "a ticket works once");
    let (s, _) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/db/photos/list"),
        Some(&phone),
        None,
    )
    .await;
    assert_eq!(s, 403);
    let (s, _) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/db/notes/list"),
        Some(&phone),
        None,
    )
    .await;
    assert_eq!(s, 200);
    let (_, me) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/me"),
        Some(&phone),
        None,
    )
    .await;
    assert_eq!(
        me["account"]["email"],
        Value::Null,
        "a database token does not learn the email"
    );
    assert_eq!(me["usage"]["databases"], 1);
    let (s, _) = call(
        reqwest::Method::POST,
        format!("{base}/api/hub/databases"),
        Some(&phone),
        Some(json!({"name": "x"})),
    )
    .await;
    assert_eq!(s, 403);
    let (s, list) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/databases"),
        Some(&phone),
        None,
    )
    .await;
    assert_eq!((s, list["items"].as_array().unwrap().len()), (200, 1));

    // Revoking the phone stops its token at once.
    let (_, devices) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/devices"),
        Some(&token),
        None,
    )
    .await;
    let phone_id = devices["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["label"] == "Phone")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (s, _) = call(
        reqwest::Method::DELETE,
        format!("{base}/api/hub/devices/{phone_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(s, 204);
    let (s, _) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/db/notes/list"),
        Some(&phone),
        None,
    )
    .await;
    assert_eq!(s, 401);

    // Deleting a database takes its replica.
    let (s, _) = call(
        reqwest::Method::DELETE,
        format!("{base}/api/hub/databases/photos"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(s, 204);
    let (s, _) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/db/photos/list"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(s, 404);
    h.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn conditional_writes_keys_limits_and_pushes() {
    let (h, base, dir) = hub("objects", Signup::Open, 4096).await;
    let token = sign_in(&h, &base, "dev@x.co").await;
    call(
        reqwest::Method::POST,
        format!("{base}/api/hub/databases"),
        Some(&token),
        Some(json!({"name": "notes"})),
    )
    .await;
    let c = reqwest::Client::new();
    let put = |key: &str, body: &'static [u8], cond: (&'static str, String)| {
        c.put(format!("{base}/api/hub/db/notes/o/{key}"))
            .bearer_auth(&token)
            .header(cond.0, cond.1)
            .body(body)
            .send()
    };
    let mut events = c
        .get(format!("{base}/api/hub/db/notes/events"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let hello = String::from_utf8(events.chunk().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(hello.starts_with("event: connected"), "{hello}");

    let r = put(
        "moul/head.json",
        br#"{"txid":1}"#,
        ("If-None-Match", "*".into()),
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let etag = r.headers()["etag"].to_str().unwrap().to_owned();
    let r = put(
        "moul/head.json",
        br#"{"txid":9}"#,
        ("If-None-Match", "*".into()),
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 412, "the key exists");
    let r = put(
        "moul/head.json",
        br#"{"txid":2}"#,
        ("If-Match", "\"stale\"".into()),
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 412);
    let r = put("moul/head.json", br#"{"txid":2}"#, ("If-Match", etag))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut pushed = String::new();
    while pushed.matches("event: head").count() < 2 {
        let chunk = tokio::time::timeout(Duration::from_secs(5), events.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        pushed.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert!(pushed.contains(r#"data: {"txid":2}"#), "{pushed}");

    let r = put("../escape", b"x", ("If-None-Match", "*".into()))
        .await
        .unwrap();
    assert_ne!(r.status(), 200);
    let r = put("moul/other.json", b"x", ("If-None-Match", "*".into()))
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    // Uploads at once cannot all slip under the limit.
    static PART: [u8; 1500] = [1u8; 1500];
    let puts = (0..4).map(|i| {
        c.put(format!(
            "{base}/api/hub/db/notes/o/ltx/0/000000000000000{n}-000000000000000{n}.ltx",
            n = i + 2
        ))
        .bearer_auth(&token)
        .header("If-None-Match", "*")
        .body(&PART[..])
        .send()
    });
    let codes: Vec<u16> = futures_util::future::join_all(puts)
        .await
        .into_iter()
        .map(|r| r.unwrap().status().as_u16())
        .collect();
    assert!(
        codes.iter().filter(|c| **c == 200).count() <= 2,
        "{codes:?}"
    );
    assert!(codes.contains(&403), "{codes:?}");

    // The account holds at most 4096 bytes.
    static BIG: [u8; 5000] = [7u8; 5000];
    let r = c
        .put(format!(
            "{base}/api/hub/db/notes/o/ltx/0/0000000000000001-0000000000000001.ltx"
        ))
        .bearer_auth(&token)
        .header("If-None-Match", "*")
        .body(&BIG[..])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    h.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// A device: its stores in memory and its sync through the hub, with a
/// second, push-only sync for its writer-only store as a server has.
struct Device {
    store: Store,
    capture: Capture,
    sync: Sync,
    writer_capture: Capture,
    writer_only: Sync,
}

impl Device {
    fn new(base: &str, token: &str, follow_only: bool) -> Device {
        let capture = Capture::open(MemoryStorage::new(), "main.db").unwrap();
        let writer_capture = Capture::open(MemoryStorage::new(), "writer.db").unwrap();
        let volume = Volume::open_observed(
            MemoryStorage::new(),
            moul_vfs::Fanout::new()
                .with(capture.clone())
                .with(writer_capture.clone()),
        )
        .unwrap();
        let store = Store::open_follower(volume).unwrap();
        let replica = || {
            HubReplica::with_default_client(Remote {
                url: base.to_owned(),
                database: "notes".into(),
                token: token.to_owned(),
            })
            .unwrap()
        };
        let sync = Sync::new(
            replica(),
            Options {
                follow_only,
                ..Options::default()
            },
        );
        let writer_only = Sync::new(
            moul_sync::Prefixed::writer_only(replica()),
            Options {
                push_only: true,
                ..Options::default()
            },
        );
        Device {
            store,
            capture,
            sync,
            writer_capture,
            writer_only,
        }
    }

    fn push_writer_only(&mut self) {
        let mut local = moul_sync::WriterOnlyLocal {
            store: &mut self.store,
            capture: self.writer_capture.clone(),
            conflicts: None,
        };
        block_on(self.writer_only.sync(&mut local)).unwrap();
    }

    fn round(&mut self) {
        let mut local = StoreLocal {
            store: &mut self.store,
            capture: self.capture.clone(),
            conflicts: None,
        };
        block_on(self.sync.sync(&mut local)).unwrap();
    }
}

#[tokio::test]
async fn two_devices_sync_through_the_hub_and_it_compacts() {
    two_devices(hub("sync", Signup::Open, 1 << 30).await).await;
}

/// The same through a hub whose replicas are in an S3-compatible bucket,
/// when `MOUL_S3_ENDPOINT` names one (CI runs RustFS).
#[tokio::test]
async fn two_devices_sync_through_a_hub_on_s3() {
    let Ok(endpoint) = std::env::var("MOUL_S3_ENDPOINT") else {
        assert!(
            std::env::var("MOUL_REQUIRE_S3").is_err(),
            "MOUL_REQUIRE_S3 is set but MOUL_S3_ENDPOINT is not"
        );
        eprintln!("skipping: MOUL_S3_ENDPOINT is not set");
        return;
    };
    let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
    let store = moul_hub::store::Store::S3(moul_sync::S3Config {
        endpoint,
        region: var("MOUL_S3_REGION", "us-east-1"),
        bucket: var("MOUL_S3_BUCKET", "moul-test"),
        prefix: format!("hub-{}", moul_sync::new_id()),
        access_key_id: var("MOUL_S3_ACCESS_KEY_ID", "moul-test"),
        secret_access_key: var("MOUL_S3_SECRET_ACCESS_KEY", "moul-test-secret"),
        session_token: None,
        path_style: var("MOUL_S3_PATH_STYLE", "true") != "false",
    });
    two_devices(hub_on("s3", Signup::Open, 1 << 30, Some(store)).await).await;
}

async fn two_devices((h, base, dir): (RunningHub, String, PathBuf)) {
    let token = sign_in(&h, &base, "dev@x.co").await;
    call(
        reqwest::Method::POST,
        format!("{base}/api/hub/databases"),
        Some(&token),
        Some(json!({"name": "notes"})),
    )
    .await;
    let (b, t) = (base.clone(), token.clone());
    let rows = tokio::task::spawn_blocking(move || {
        let mut a = Device::new(&b, &t, false);
        a.store
            .conn()
            .execute_batch(
                "CREATE TABLE t (x); INSERT INTO t VALUES (1);
                 CREATE TABLE writer.secrets (x); INSERT INTO secrets VALUES (1);",
            )
            .unwrap();
        a.round();
        a.push_writer_only();
        a.store
            .conn()
            .execute_batch("INSERT INTO t VALUES (2); INSERT INTO secrets VALUES (2);")
            .unwrap();
        a.round();
        a.push_writer_only();
        let mut phone = Device::new(&b, &t, true);
        phone.round();
        phone
            .store
            .conn()
            .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(rows, 2, "the follower has the writer's rows");
    let (_, list) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/databases"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(list["items"][0]["head"]["txid"], 2);
    // The devices left the replica alone; the hub compacts it.
    let (_, before) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/db/notes/list?prefix=ltx/1/"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(before["items"].as_array().unwrap().len(), 0);
    assert_eq!(moul_hub::api::maintain_due(&h.state, &h.hub).await, 1);
    let (_, after) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/db/notes/list?prefix=ltx/1/"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(after["items"].as_array().unwrap().len(), 1, "{after}");
    // And the writer-only store a server pushes beside it, the same way.
    let (_, after) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/db/notes/list?prefix=moul/writer-only/ltx/1/"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(after["items"].as_array().unwrap().len(), 1, "{after}");
    let (_, list) = call(
        reqwest::Method::GET,
        format!("{base}/api/hub/databases"),
        Some(&token),
        None,
    )
    .await;
    assert!(list["items"][0]["bytes"].as_i64().unwrap() > 0);
    h.stop().await;
    let _ = std::fs::remove_dir_all(dir);
}
