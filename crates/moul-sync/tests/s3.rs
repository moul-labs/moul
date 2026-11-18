//! The S3 transport against a real S3-compatible store.
//!
//! Set `MOUL_S3_ENDPOINT` (and optionally `MOUL_S3_BUCKET`, `MOUL_S3_REGION`,
//! `MOUL_S3_ACCESS_KEY_ID`, `MOUL_S3_SECRET_ACCESS_KEY`) to run these; CI runs
//! them against MinIO. Without it they pass without doing anything, unless
//! `MOUL_REQUIRE_S3=1`. Each test works under its own prefix.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{Clock, Device, assert_converged};
use moul_sync::{
    Condition, ErrorKind, Put, Replica, Retention, Role, S3Config, S3Replica, block_on,
};

fn config(test: &str) -> Option<S3Config> {
    let Ok(endpoint) = std::env::var("MOUL_S3_ENDPOINT") else {
        assert!(
            std::env::var("MOUL_REQUIRE_S3").is_err(),
            "MOUL_REQUIRE_S3 is set but MOUL_S3_ENDPOINT is not"
        );
        eprintln!("skipping {test}: MOUL_S3_ENDPOINT is not set");
        return None;
    };
    let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
    Some(S3Config {
        endpoint,
        region: var("MOUL_S3_REGION", "us-east-1"),
        bucket: var("MOUL_S3_BUCKET", "moul-test"),
        prefix: format!("tests/{test}-{}", moul_sync::new_id()),
        access_key_id: var("MOUL_S3_ACCESS_KEY_ID", "moul-test"),
        secret_access_key: var("MOUL_S3_SECRET_ACCESS_KEY", "moul-test-secret"),
        session_token: None,
        path_style: var("MOUL_S3_PATH_STYLE", "true") != "false",
    })
}

fn replica(c: &S3Config) -> S3Replica {
    S3Replica::with_default_client(c.clone()).unwrap()
}

#[test]
fn conditional_writes_listing_and_deletes() {
    let Some(c) = config("contract") else { return };
    let r = replica(&c);
    block_on(async {
        assert_eq!(r.get("moul/head.json").await.unwrap(), None);
        let Put::Written { etag } = r
            .put("moul/head.json", b"one", Condition::IfNoneMatch)
            .await
            .unwrap()
        else {
            panic!("first create refused")
        };
        assert!(!etag.is_empty());
        assert_eq!(
            r.put("moul/head.json", b"two", Condition::IfNoneMatch)
                .await
                .unwrap(),
            Put::Refused
        );
        let got = r.get("moul/head.json").await.unwrap().unwrap();
        assert_eq!(
            (got.bytes.as_slice(), got.etag.as_str()),
            (&b"one"[..], etag.as_str())
        );
        let Put::Written { etag: e2 } = r
            .put("moul/head.json", b"three", Condition::IfMatch(etag.clone()))
            .await
            .unwrap()
        else {
            panic!("swap with the current etag refused")
        };
        assert_eq!(
            r.put("moul/head.json", b"four", Condition::IfMatch(etag))
                .await
                .unwrap(),
            Put::Refused,
            "a stale etag must be refused"
        );
        assert_eq!(
            r.put("moul/missing.json", b"x", Condition::IfMatch(e2))
                .await
                .unwrap(),
            Put::Refused
        );
        for t in 1..=3u64 {
            let key = moul_sync::layout::ltx_key(0, moul_ltx::Txid(t), moul_ltx::Txid(t));
            r.put(&key, &vec![t as u8; 100 * t as usize], Condition::None)
                .await
                .unwrap();
        }
        let listed = r.list("ltx/").await.unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[2].size, 300);
        assert!(listed[0].modified > 1_700_000_000_000);
        assert_eq!(r.list("").await.unwrap().len(), 4);
        r.delete(&listed[0].key).await.unwrap();
        r.delete(&listed[0].key).await.unwrap();
        assert_eq!(r.list("ltx/").await.unwrap().len(), 2);
    });
}

#[test]
fn only_one_of_many_racing_creates_wins() {
    let Some(c) = config("race") else { return };
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let c = c.clone();
            std::thread::spawn(move || {
                let r = replica(&c);
                block_on(r.put(
                    "ltx/0/0000000000000002-0000000000000002.ltx",
                    format!("device {i}").as_bytes(),
                    Condition::IfNoneMatch,
                ))
                .unwrap()
            })
        })
        .collect();
    let won = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|p| matches!(p, Put::Written { .. }))
        .count();
    assert_eq!(won, 1);
}

#[test]
fn wrong_credentials_are_denied() {
    let Some(mut c) = config("denied") else {
        return;
    };
    c.secret_access_key = "not the secret".into();
    let err = block_on(replica(&c).get("moul/head.json")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Denied, "{err}");
}

#[test]
fn devices_sync_hand_off_and_keep_conflicts_through_a_bucket() {
    let Some(c) = config("devices") else { return };
    let clock = Clock::new();
    let new = |name: &str| {
        let c = c.clone();
        Device::new(
            name,
            move || Box::new(replica(&c)),
            &clock,
            Retention::default(),
        )
    };
    let mut a = new("laptop");
    a.exec("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');");
    a.connect().unwrap();
    let mut b = new("phone");
    b.connect().unwrap();
    assert_eq!((a.role(), b.role()), (Role::Writer, Role::Follower));

    a.exec("INSERT INTO t VALUES ('two')");
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!(b.rows(), ["one", "two"]);

    a.exec("INSERT INTO t VALUES ('offline')");
    b.take_over().unwrap();
    b.exec("INSERT INTO t VALUES ('phone')");
    b.sync().unwrap();
    a.sync().unwrap();
    assert_eq!(a.role(), Role::Follower);
    assert_eq!(a.rows(), ["one", "two", "phone"]);
    assert_eq!(a.conflict_copies().len(), 1);

    // Compaction, a snapshot and pruning against the real store.
    clock.advance(25 * 60 * 60 * 1000);
    b.sync().unwrap();
    let keys: Vec<String> = block_on(replica(&c).list("ltx/"))
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    assert!(keys.iter().any(|k| k.starts_with("ltx/1/")), "{keys:?}");
    assert!(keys.iter().any(|k| k.starts_with("ltx/9/")), "{keys:?}");
    a.sync().unwrap();
    assert_converged(&replica(&c), &[&a, &b]);
}
