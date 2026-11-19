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

/// A replica that carries a writer-only store beside the shared one, as a
/// server pushes it: the fixture's files as the shared store, and its files
/// up to TXID 3 as the writer-only store, so the two restore differently.
fn replica_with_writer_only(dir: &Path) -> PathBuf {
    let replica = dir.join("replica");
    let from = fixture().join("replica").join("ltx");
    for level in std::fs::read_dir(&from).unwrap() {
        let level = level.unwrap();
        for file in std::fs::read_dir(level.path()).unwrap() {
            let file = file.unwrap();
            let name = file.file_name().to_string_lossy().into_owned();
            let max = u64::from_str_radix(&name[17..33], 16).unwrap();
            let mut targets = vec![replica.join("ltx")];
            if max <= 3 {
                targets.push(replica.join("moul/writer-only/ltx"));
            }
            for t in targets {
                let to = t.join(level.file_name());
                std::fs::create_dir_all(&to).unwrap();
                std::fs::copy(file.path(), to.join(&name)).unwrap();
            }
        }
    }
    replica
}

#[test]
fn restore_brings_back_the_writer_only_store() {
    let dir = scratch("restore-writer-only");
    let replica = replica_with_writer_only(&dir);
    let r = replica.to_str().unwrap();
    let expected = std::fs::read(fixture().join("expected.db")).unwrap();
    let at3 = dir.join("at3.db");
    let (ok, text) = moul(&["restore", r, "-o", at3.to_str().unwrap(), "--txid", "3"]);
    assert!(ok, "{text}");
    let at3 = std::fs::read(&at3).unwrap();

    // One store to a file: the shared store by default, as before.
    let out = dir.join("shared.db");
    let (ok, text) = moul(&["restore", r, "-o", out.to_str().unwrap()]);
    assert!(ok && text.contains("restored TXID 7"), "{text}");
    assert!(std::fs::read(&out).unwrap() == expected);
    let out = dir.join("writer.db");
    let (ok, text) = moul(&["restore", r, "--writer-only", "-o", out.to_str().unwrap()]);
    assert!(
        ok && text.contains("restored the writer-only store at TXID 3"),
        "{text}"
    );
    assert!(std::fs::read(&out).unwrap() == at3);
    let (ok, text) = moul(&[
        "restore",
        r,
        "--writer-only",
        "-o",
        out.to_str().unwrap(),
        "-f",
        "--txid",
        "2",
    ]);
    assert!(ok && text.contains("at TXID 2"), "{text}");

    // Both stores into a folder, ready for `moul serve`.
    let folder = dir.join("served");
    let arg = format!("{}/", folder.display());
    let (ok, text) = moul(&["restore", r, "-o", &arg]);
    assert!(ok, "{text}");
    assert!(std::fs::read(folder.join("main.db")).unwrap() == expected);
    assert!(std::fs::read(folder.join("writer.db")).unwrap() == at3);
    let mut left: Vec<String> = std::fs::read_dir(&folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["main.db", "writer.db"]);
    let f = folder.to_str().unwrap();
    let (ok, text) = moul(&["restore", r, "-o", f]);
    assert!(!ok && text.contains("--force"), "{text}");
    let (ok, text) = moul(&["restore", r, "-o", f, "--force"]);
    assert!(ok, "{text}");
    let (ok, text) = moul(&["restore", r, "-o", f, "--force", "--txid", "3"]);
    assert!(!ok && text.contains("--txid"), "{text}");
    let (ok, text) = moul(&["restore", r, "-o", f, "--force", "--writer-only"]);
    assert!(!ok && text.contains("--writer-only"), "{text}");
    // Never over a folder that holds anything else, such as a commit log.
    std::fs::write(folder.join("commit.log"), b"").unwrap();
    let (ok, text) = moul(&["restore", r, "-o", f, "--force"]);
    assert!(!ok && text.contains("commit.log"), "{text}");

    // A replica without one says so, and a folder gets the shared store.
    let plain = fixture().join("replica");
    let p = plain.to_str().unwrap();
    let (ok, text) = moul(&[
        "restore",
        p,
        "--writer-only",
        "-o",
        out.to_str().unwrap(),
        "-f",
    ]);
    assert!(!ok && text.contains("holds no writer-only store"), "{text}");
    let bare = dir.join("bare");
    let (ok, text) = moul(&["restore", p, "-o", &format!("{}/", bare.display())]);
    assert!(ok && text.contains("holds no writer-only store"), "{text}");
    assert!(bare.join("main.db").is_file() && !bare.join("writer.db").exists());

    let (ok, text) = moul(&["info", r]);
    assert!(ok && text.contains("writer-only  TXID 3"), "{text}");
    let (ok, text) = moul(&["info", p]);
    assert!(ok && text.contains("writer-only  none"), "{text}");
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
