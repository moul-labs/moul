//! `moul restore`, `moul pull` and `moul info` on the replica Litestream
//! wrote in `fixtures/litestream`.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::process::Command;

fn moul(args: &[&str]) -> (bool, String) {
    moul_in(None, args)
}

/// Runs moul in the folder `cwd`, or in the test's own when it is `None`.
fn moul_in(cwd: Option<&Path>, args: &[&str]) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_moul"));
    cmd.args(args).env_remove("AWS_ACCESS_KEY_ID");
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/litestream")
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("moul-cli")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn restore_matches_litestream() {
    let dir = scratch("restore");
    let out = dir.join("out.db");
    let replica = fixture().join("replica");
    let (ok, text) = moul(&[
        "restore",
        replica.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{text}");
    assert!(text.contains("restored TXID 7"), "{text}");
    assert!(std::fs::read(&out).unwrap() == std::fs::read(fixture().join("expected.db")).unwrap());

    let (ok, text) = moul(&[
        "restore",
        replica.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(!ok && text.contains("--force"), "{text}");
    let (ok, text) = moul(&[
        "restore",
        replica.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--force",
        "--txid",
        "3",
    ]);
    assert!(ok && text.contains("restored TXID 3"), "{text}");
    let (ok, text) = moul(&[
        "restore",
        replica.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "-f",
        "--txid",
        "99",
    ]);
    assert!(!ok && text.contains("not available"), "{text}");
}

/// Output names SQLite would read as URI syntax if the integrity check did
/// not escape them.
fn awkward_names() -> Vec<&'static str> {
    let mut names = vec!["db#2.sqlite", "50%41 off.db", "100%.db", "a b.db"];
    if cfg!(unix) {
        names.push("what?immutable=0.db");
    }
    names
}

#[test]
fn restore_to_any_file_name() {
    let dir = scratch("restore-names");
    let replica = fixture().join("replica");
    let expected = std::fs::read(fixture().join("expected.db")).unwrap();
    for name in awkward_names() {
        // Once by absolute path, once by a path relative to moul's folder.
        let out = dir.join(name);
        let (ok, text) = moul(&[
            "restore",
            replica.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ]);
        assert!(ok && text.contains("restored TXID 7"), "{name}: {text}");
        assert!(std::fs::read(&out).unwrap() == expected, "{name}");
        let (ok, text) = moul_in(
            Some(&dir),
            &["restore", replica.to_str().unwrap(), "-o", name, "--force"],
        );
        assert!(ok && text.contains("restored TXID 7"), "{name}: {text}");
        assert!(std::fs::read(&out).unwrap() == expected, "{name}");
    }
    // Only the restored files: no temporary, -wal or -shm file is left.
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    let mut want: Vec<String> = awkward_names().into_iter().map(String::from).collect();
    want.sort();
    assert_eq!(left, want);
}

#[test]
fn pull_refuses_a_bad_every() {
    let dir = scratch("pull-every");
    let copy = dir.join("copy");
    let replica = fixture().join("replica");
    let (replica, copy_s) = (replica.to_str().unwrap(), copy.to_str().unwrap());
    let bad = [
        "inf", "infinity", "-inf", "NaN", "1e20", "86401", "-1", "0", "0.1", "soon",
    ];
    for every in bad {
        let (ok, text) = moul(&["pull", replica, copy_s, "--every", every]);
        assert!(!ok && text.contains("from 0.2 to 86400"), "{every}: {text}");
        assert!(!copy.exists(), "{every}: pulled before refusing");
    }
    // These sent a watching pull to sleep on a value Duration cannot hold.
    for every in ["inf", "1e20"] {
        let (ok, text) = moul(&["pull", replica, copy_s, "--watch", "--every", every]);
        assert!(!ok && text.contains("from 0.2 to 86400"), "{every}: {text}");
        assert!(!text.contains("panicked"), "{every}: {text}");
        assert!(!copy.exists(), "{every}: pulled before refusing");
    }
    let (ok, text) = moul(&["pull", replica, copy_s, "--every=0.2"]);
    assert!(ok && text.contains("at TXID 7"), "{text}");
}

#[test]
fn pull_keeps_a_follower_copy() {
    let dir = scratch("pull").join("copy");
    let replica = fixture().join("replica");
    for _ in 0..2 {
        let (ok, text) = moul(&["pull", replica.to_str().unwrap(), dir.to_str().unwrap()]);
        assert!(ok, "{text}");
        assert!(text.contains("at TXID 7"), "{text}");
    }
    assert!(
        std::fs::read(dir.join("main.db")).unwrap()
            == std::fs::read(fixture().join("expected.db")).unwrap()
    );
}

#[test]
fn info_and_errors() {
    let replica = fixture().join("replica");
    let (ok, text) = moul(&["info", replica.to_str().unwrap()]);
    assert!(ok, "{text}");
    assert!(text.contains("restores  TXIDs 1 to 7"), "{text}");
    let (ok, text) = moul(&["info", "s3://bucket/x"]);
    assert!(!ok && text.contains("AWS_ACCESS_KEY_ID"), "{text}");
    let (ok, text) = moul(&["frob"]);
    assert!(!ok && text.contains("unknown command"), "{text}");
    let (ok, text) = moul(&["help"]);
    assert!(ok && text.contains("moul restore"), "{text}");
}

#[test]
fn schema_and_typegen_read_a_go_database() {
    let dir = scratch("typegen");
    let db = dir.join("go.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    moul_fixtures::Fixtures::default()
        .db("cbc2a77b5c6e0079")
        .unwrap()
        .load_into(&conn)
        .unwrap();
    drop(conn);

    let schema = dir.join("moul.schema.json");
    let (ok, text) = moul(&[
        "schema",
        db.to_str().unwrap(),
        "-o",
        schema.to_str().unwrap(),
    ]);
    assert!(ok, "{text}");
    let file: serde_json::Value = serde_json::from_slice(&std::fs::read(&schema).unwrap()).unwrap();
    let names: Vec<&str> = file["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["categories", "events", "posts", "tasks_queue", "users"]
    );

    let (ok, ts) = moul(&["typegen", schema.to_str().unwrap()]);
    assert!(ok, "{ts}");
    assert!(
        ts.contains("export interface UsersRecord extends AuthSystemFields {"),
        "{ts}"
    );
    assert!(ts.contains("\"posts\": PostsRecord;"), "{ts}");
    let (ok, from_db) = moul(&["typegen", db.to_str().unwrap()]);
    assert!(ok, "{from_db}");
    assert_eq!(from_db, ts);
}
