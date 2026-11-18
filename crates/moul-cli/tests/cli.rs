//! `moul restore`, `moul pull` and `moul info` on the replica Litestream
//! wrote in `fixtures/litestream`.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::process::Command;

fn moul(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_moul"))
        .args(args)
        .env_remove("AWS_ACCESS_KEY_ID")
        .output()
        .unwrap();
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
