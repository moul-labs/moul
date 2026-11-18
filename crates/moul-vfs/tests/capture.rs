//! Commit capture: the level-0 files always add up to the database.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use common::{connect, integrity};
use moul_ltx::{Ltx, Txid, restore};
use moul_vfs::{Capture, ErrorKind, MemoryStorage, Volume};
use rusqlite::{Connection, OpenFlags, params};

/// Restores the database from every file `capture` has written.
fn restore_captured(capture: &Capture) -> Vec<u8> {
    let last = capture.position().txid.0;
    let files: Vec<Vec<u8>> = (1..=last).map(|t| capture.read(Txid(t)).unwrap()).collect();
    let decoded: Vec<Ltx<'_>> = files.iter().map(|b| Ltx::decode(b).unwrap()).collect();
    restore::restore_to_vec(&decoded).unwrap()
}

fn assert_replica_matches(capture: &Capture, volume: &Volume, what: &str) {
    let restored = restore_captured(capture);
    let exported = volume.export("main.db").unwrap();
    assert!(
        restored == exported,
        "{what}: restored {} bytes, file has {}",
        restored.len(),
        exported.len()
    );
}

fn open(volume_storage: &MemoryStorage, ltx: &MemoryStorage) -> (Capture, Volume) {
    let capture = Capture::open(ltx.clone(), "main.db")
        .unwrap()
        .with_clock(|| 1_700_000_000_000);
    let volume = Volume::open_observed(volume_storage.clone(), capture.clone()).unwrap();
    (capture, volume)
}

fn connect_with_page_size(volume: &Volume, page_size: u32) -> Connection {
    let conn = Connection::open_with_flags_and_vfs(
        "main.db",
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        volume.vfs_name(),
    )
    .unwrap();
    conn.execute_batch(&format!(
        "PRAGMA page_size = {page_size}; PRAGMA journal_mode = MEMORY; PRAGMA synchronous = OFF;"
    ))
    .unwrap();
    conn
}

#[test]
fn every_commit_to_main_becomes_one_file() {
    let (storage, ltx) = (MemoryStorage::new(), MemoryStorage::new());
    let (capture, volume) = open(&storage, &ltx);
    let conn = connect(&volume).unwrap();

    conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY, v BLOB)")
        .unwrap();
    assert_eq!(capture.position().txid, Txid(1));
    let first = capture.read(Txid(1)).unwrap();
    let first = Ltx::decode(&first).unwrap();
    assert!(first.header().is_snapshot());
    assert_eq!(first.header().timestamp, 1_700_000_000_000);
    assert_replica_matches(&capture, &volume, "after create");

    // A commit to the writer-only store alone writes nothing.
    conn.execute_batch("CREATE TABLE writer.secrets (k INTEGER PRIMARY KEY, s TEXT)")
        .unwrap();
    assert_eq!(capture.position().txid, Txid(1));

    for i in 0..20 {
        conn.execute(
            "INSERT INTO kv VALUES (?1, ?2)",
            params![i, vec![i as u8; 900]],
        )
        .unwrap();
        assert_replica_matches(&capture, &volume, &format!("insert {i}"));
    }
    assert_eq!(capture.position().txid, Txid(21));
    // Later files hold only what changed.
    let small = capture.read(Txid(21)).unwrap();
    let small = Ltx::decode(&small).unwrap();
    assert!(small.page_count() < 5, "{} pages", small.page_count());

    conn.execute_batch(
        "BEGIN;
         UPDATE kv SET v = x'00' WHERE k < 10;
         INSERT INTO secrets VALUES (1, 'hash');
         COMMIT;",
    )
    .unwrap();
    assert_replica_matches(&capture, &volume, "update across stores");

    // The database shrinks.
    conn.execute_batch("DELETE FROM kv WHERE k > 2; VACUUM;")
        .unwrap();
    assert_replica_matches(&capture, &volume, "after vacuum");
    let shrunk = capture.read(capture.position().txid).unwrap();
    let shrunk = Ltx::decode(&shrunk).unwrap();
    assert_eq!(
        shrunk.header().commit as usize * 4096,
        volume.export("main.db").unwrap().len()
    );
    assert_eq!(integrity(&conn), "ok");
}

#[test]
fn page_sizes_from_512_to_65536() {
    for page_size in [512, 1024, 4096, 8192, 65_536] {
        let (storage, ltx) = (MemoryStorage::new(), MemoryStorage::new());
        let (capture, volume) = open(&storage, &ltx);
        let conn = connect_with_page_size(&volume, page_size);
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)")
            .unwrap();
        for i in 0..30 {
            conn.execute(
                "INSERT INTO t (body) VALUES (?1)",
                params![format!("{i}").repeat(300)],
            )
            .unwrap();
        }
        conn.execute_batch("DELETE FROM t WHERE id % 3 = 0; VACUUM;")
            .unwrap();
        let f = capture.read(Txid(1)).unwrap();
        assert_eq!(Ltx::decode(&f).unwrap().header().page_size, page_size);
        assert_replica_matches(&capture, &volume, &format!("page size {page_size}"));
    }
}

#[test]
fn commits_made_without_capture_are_caught_up_with_a_whole_copy() {
    let (storage, ltx) = (MemoryStorage::new(), MemoryStorage::new());
    {
        let (capture, volume) = open(&storage, &ltx);
        let conn = connect(&volume).unwrap();
        conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        assert_eq!(capture.position().txid, Txid(1));
    }
    {
        // Someone opens the volume without capture and writes.
        let volume = Volume::open(storage.clone()).unwrap();
        let conn = connect(&volume).unwrap();
        for i in 0..50 {
            conn.execute("INSERT INTO kv VALUES (?1, 'x')", params![i])
                .unwrap();
        }
    }
    let (capture, volume) = open(&storage, &ltx);
    let conn = connect(&volume).unwrap();
    conn.execute("INSERT INTO kv VALUES (1000, 'y')", [])
        .unwrap();
    assert_eq!(capture.position().txid, Txid(2));
    let copy = capture.read(Txid(2)).unwrap();
    let copy = Ltx::decode(&copy).unwrap();
    assert_eq!(
        copy.page_count() as u32,
        copy.header().commit,
        "a whole copy"
    );
    assert_replica_matches(&capture, &volume, "after the gap");
}

#[test]
fn an_existing_database_is_captured_whole() {
    let (storage, ltx) = (MemoryStorage::new(), MemoryStorage::new());
    {
        let volume = Volume::open(storage.clone()).unwrap();
        let conn = connect(&volume).unwrap();
        conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY); INSERT INTO kv VALUES (1);")
            .unwrap();
    }
    let (capture, volume) = open(&storage, &ltx);
    assert_eq!(capture.snapshot_existing(&volume).unwrap(), Some(Txid(1)));
    assert_eq!(capture.snapshot_existing(&volume).unwrap(), None);
    assert_replica_matches(&capture, &volume, "existing");
    let conn = connect(&volume).unwrap();
    conn.execute("INSERT INTO kv VALUES (2)", []).unwrap();
    assert_eq!(capture.position().txid, Txid(2));
    assert_replica_matches(&capture, &volume, "after one more");
}

#[test]
fn a_failed_capture_stops_the_volume_and_is_retried_on_reopen() {
    let (storage, ltx) = (MemoryStorage::new(), MemoryStorage::new());
    {
        let (capture, volume) = open(&storage, &ltx);
        let conn = connect(&volume).unwrap();
        conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        assert_eq!(capture.position().txid, Txid(1));

        ltx.fail_at(Some(ltx.steps()));
        let err = conn
            .execute("INSERT INTO kv VALUES (1, 'kept')", [])
            .unwrap_err();
        assert!(volume.is_poisoned(), "{err}");
        assert!(
            conn.execute("INSERT INTO kv VALUES (2, 'refused')", [])
                .is_err()
        );
        assert_eq!(capture.position().txid, Txid(1));
        drop(conn);
    }
    ltx.fail_at(None);
    let (capture, volume) = open(&storage, &ltx);
    // The durable commit was handed over again on open.
    assert_eq!(capture.position().txid, Txid(2));
    assert_replica_matches(&capture, &volume, "after reopen");
    let conn = connect(&volume).unwrap();
    let v: String = conn
        .query_row("SELECT v FROM kv WHERE k = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, "kept");
}

#[test]
fn a_failing_capture_fails_the_open() {
    let (storage, ltx) = (MemoryStorage::new(), MemoryStorage::new());
    {
        let (_capture, volume) = open(&storage, &ltx);
        let conn = connect(&volume).unwrap();
        conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY)")
            .unwrap();
        ltx.fail_at(Some(ltx.steps()));
        assert!(conn.execute("INSERT INTO kv VALUES (1)", []).is_err());
    }
    let capture = Capture::open(ltx.clone(), "main.db").unwrap();
    let err = Volume::open_observed(storage.clone(), capture).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Io, "{err}");
}
