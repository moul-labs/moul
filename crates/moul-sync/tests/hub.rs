//! Devices that sync through a Moul Hub: the hub transport against a hub
//! that serves the protocol of `docs/hub.md` over an in-memory replica.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{Clock, Device, assert_converged};
use moul_sync::s3::{HttpClient, HttpRequest, HttpResponse};
use moul_sync::{
    BoxFuture, Condition, ErrorKind, HubConfig, HubReplica, MemoryReplica, Put, Replica, Retention,
    Role,
};

const MINUTE: i64 = 60_000;

/// A hub in a test: the replica calls of one database, behind a token.
#[derive(Clone)]
struct FakeHub {
    replica: MemoryReplica,
    token: Rc<RefCell<String>>,
    /// Every request, as `METHOD path`.
    log: Rc<RefCell<Vec<String>>>,
}

fn json(status: u16, value: serde_json::Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&value).unwrap(),
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(h) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

impl FakeHub {
    fn new(replica: &MemoryReplica) -> FakeHub {
        FakeHub {
            replica: replica.clone(),
            token: Rc::new(RefCell::new("mh_laptop".into())),
            log: Rc::default(),
        }
    }

    async fn serve(&self, req: HttpRequest) -> moul_sync::Result<HttpResponse> {
        let path = req
            .url
            .strip_prefix("https://hub.test/api/hub/db/notes/")
            .expect("a replica call on the notes database")
            .to_owned();
        self.log.borrow_mut().push(format!("{} {path}", req.method));
        let header = |name: &str| {
            req.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        if header("authorization") != Some(format!("Bearer {}", self.token.borrow())) {
            return Ok(json(401, serde_json::json!({ "message": "unknown token" })));
        }
        if let Some(prefix) = path.strip_prefix("list?prefix=") {
            let items: Vec<_> = self
                .replica
                .list(&decode(prefix))
                .await?
                .into_iter()
                .map(
                    |o| serde_json::json!({ "key": o.key, "size": o.size, "modified": o.modified }),
                )
                .collect();
            return Ok(json(200, serde_json::json!({ "items": items })));
        }
        let key = decode(path.strip_prefix("o/").expect("an object"));
        Ok(match req.method {
            "GET" => match self.replica.get(&key).await? {
                Some(obj) => HttpResponse {
                    status: 200,
                    headers: vec![("etag".into(), obj.etag)],
                    body: obj.bytes,
                },
                None => json(404, serde_json::json!({ "message": "no such object" })),
            },
            "PUT" => {
                let condition = match (header("if-none-match"), header("if-match")) {
                    (Some(star), None) if star == "*" => Condition::IfNoneMatch,
                    (None, Some(etag)) => Condition::IfMatch(etag),
                    (None, None) => Condition::None,
                    other => panic!("unexpected conditions {other:?}"),
                };
                match self.replica.put(&key, &req.body, condition).await? {
                    Put::Written { etag } => HttpResponse {
                        status: 200,
                        headers: vec![("etag".into(), etag.clone())],
                        body: serde_json::to_vec(&serde_json::json!({ "etag": etag })).unwrap(),
                    },
                    Put::Refused => json(412, serde_json::json!({ "message": "condition failed" })),
                }
            }
            "DELETE" => {
                self.replica.delete(&key).await?;
                HttpResponse {
                    status: 204,
                    headers: vec![],
                    body: vec![],
                }
            }
            other => panic!("unexpected method {other}"),
        })
    }
}

impl HttpClient for FakeHub {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, moul_sync::Result<HttpResponse>> {
        Box::pin(self.serve(request))
    }
}

fn device(name: &str, hub: &FakeHub, token: &str, clock: &Clock, r: Retention) -> Device {
    let hub = hub.clone();
    let token = token.to_owned();
    Device::new(
        name,
        move || {
            let config = HubConfig {
                url: "https://hub.test".into(),
                database: "notes".into(),
                token: token.clone(),
            };
            Box::new(HubReplica::new(config, hub.clone()).unwrap())
        },
        clock,
        r,
    )
}

fn short() -> Retention {
    Retention {
        level1_every: 5 * MINUTE,
        level2_every: 10 * MINUTE,
        snapshot_every: 20 * MINUTE,
        compacted_kept: 30 * MINUTE,
        history: 60 * MINUTE,
    }
}

#[test]
fn devices_sync_through_a_hub_and_leave_compaction_to_it() {
    let clock = Clock::new();
    let replica = MemoryReplica::new();
    replica.set_now(clock.now());
    let hub = FakeHub::new(&replica);

    let mut a = device("laptop", &hub, "mh_laptop", &clock, short());
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('first');");
    a.connect().unwrap();
    assert_eq!(a.role(), Role::Writer);
    assert_eq!(a.status().replica, "moul+https://hub.test/notes");

    let mut b = device("phone", &hub, "mh_laptop", &clock, short());
    b.connect().unwrap();
    assert_eq!(b.role(), Role::Follower);
    assert_eq!(b.rows(), ["first"]);

    // Three hours of writing, well past every compaction and retention
    // interval: the device does none of it.
    for i in 0..36 {
        clock.advance(5 * MINUTE);
        replica.set_now(clock.now());
        a.exec(&format!("INSERT INTO t VALUES ('row {i}')"));
        a.sync().unwrap();
    }
    b.sync().unwrap();
    assert_eq!(b.rows().len(), 37);
    assert!(a.sync.last_report().is_none(), "no maintenance pass ran");
    let keys = replica.keys();
    assert!(
        keys.iter()
            .filter(|k| k.starts_with("ltx/"))
            .all(|k| k.starts_with("ltx/0/")),
        "only level-0 files: {keys:?}"
    );
    assert!(
        !hub.log.borrow().iter().any(|l| l.starts_with("DELETE")),
        "nothing deleted"
    );
    assert_converged(&replica, &[&a, &b]);

    // Writing moves by hand-off through the hub's conditional writes.
    b.take_over().unwrap();
    assert_eq!(b.role(), Role::Writer);
    b.exec("INSERT INTO t VALUES ('from the phone')");
    b.sync().unwrap();
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Follower);
    assert_eq!(a.rows().last().map(String::as_str), Some("from the phone"));
    assert_converged(&replica, &[&a, &b]);
}

#[test]
fn a_revoked_token_is_refused_for_good() {
    let clock = Clock::new();
    let replica = MemoryReplica::new();
    replica.set_now(clock.now());
    let hub = FakeHub::new(&replica);
    let mut a = device("laptop", &hub, "mh_laptop", &clock, Retention::default());
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('first');");
    a.connect().unwrap();

    *hub.token.borrow_mut() = "mh_other".into();
    a.exec("INSERT INTO t VALUES ('second')");
    let e = a.sync().unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Denied);
    assert!(!e.is_transient());
    let status = a.status();
    let last = status.last_error.unwrap();
    assert!(last.contains("unknown token"), "{last}");
    assert!(!last.contains("mh_laptop"), "{last}");
    assert_eq!(status.pending, 1, "the change waits for a token that works");
}
