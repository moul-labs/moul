//! SQLite running on a volume.

#![cfg(not(target_arch = "wasm32"))]

mod common;

use std::sync::{Arc, Mutex};

use common::{connect, count, integrity};
use moul_vfs::{Commit, CommitSeq, DiskStorage, LOG_NAME, MemoryStorage, Storage, Volume};
use rusqlite::{Connection, params};

fn fill(conn: &Connection, from: i64, n: i64, size: usize) {
    conn.execute_batch("BEGIN").unwrap();
    for k in from..from + n {
        conn.execute(
            "INSERT INTO kv (k, v) VALUES (?1, ?2)",
            params![k, vec![(k % 251) as u8; size]],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
}

#[test]
fn data_survives_reopening() {
    let storage = MemoryStorage::new();
    {
        let volume = Volume::open(storage.clone()).unwrap();
        let conn = connect(&volume).unwrap();
        conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY, v BLOB)")
            .unwrap();
        fill(&conn, 0, 200, 300);
        assert_eq!(volume.last_commit(), CommitSeq(2));
        drop(conn);
    }
    assert_eq!(
        storage.file(LOG_NAME).unwrap().len(),
        128,
        "the log is cut back to its header once commits are applied"
    );
    let main = storage.file("main.db").unwrap();
    assert_eq!(&main[..16], b"SQLite format 3\0");
    assert_eq!(u16::from_be_bytes([main[16], main[17]]), 4096);
    assert_eq!(main.len() % 4096, 0);

    let volume = Volume::open(storage).unwrap();
    assert_eq!(volume.last_commit(), CommitSeq(2));
    let conn = connect(&volume).unwrap();
    assert_eq!(count(&conn, "kv"), 200);
    assert_eq!(integrity(&conn), "ok");
}

#[test]
fn a_rollback_commits_nothing_even_after_the_cache_spills() {
    let volume = Volume::open(MemoryStorage::new()).unwrap();
    let conn = connect(&volume).unwrap();
    conn.execute_batch(
        "CREATE TABLE kv (k INTEGER PRIMARY KEY, v BLOB);
         PRAGMA cache_size = 8;",
    )
    .unwrap();
    fill(&conn, 0, 10, 100);
    let before = volume.export("main.db").unwrap();
    let seq = volume.last_commit();

    // 400 rows of 3 KB is far more than an 8-page cache, so SQLite writes
    // pages to the file before the transaction ends.
    conn.execute_batch("BEGIN").unwrap();
    for k in 100..500 {
        conn.execute(
            "INSERT INTO kv (k, v) VALUES (?1, ?2)",
            params![k, vec![1u8; 3000]],
        )
        .unwrap();
    }
    conn.execute_batch("ROLLBACK").unwrap();

    assert_eq!(volume.last_commit(), seq);
    assert_eq!(volume.export("main.db").unwrap(), before);
    assert_eq!(count(&conn, "kv"), 10);
    assert_eq!(integrity(&conn), "ok");

    // The same spilling transaction commits as one commit.
    fill(&conn, 100, 400, 3000);
    assert_eq!(volume.last_commit(), CommitSeq(seq.0 + 1));
    assert_eq!(count(&conn, "kv"), 410);
    assert_eq!(integrity(&conn), "ok");
}

#[test]
fn a_transaction_across_stores_is_one_commit() {
    let volume = Volume::open(MemoryStorage::new()).unwrap();
    let commits: Arc<Mutex<Vec<Commit>>> = Arc::default();
    let seen = commits.clone();
    volume.set_observer(move |c: &Commit, _: &mut moul_vfs::Before<'_>| {
        seen.lock().unwrap().push(c.clone());
        Ok(())
    });
    let conn = connect(&volume).unwrap();
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE main.users (id TEXT PRIMARY KEY, email TEXT);
         CREATE TABLE writer.passwords (user TEXT PRIMARY KEY, hash TEXT);
         INSERT INTO users VALUES ('u1', 'a@example.com');
         INSERT INTO passwords VALUES ('u1', 'argon2...');
         COMMIT;",
    )
    .unwrap();

    let commits = commits.lock().unwrap();
    assert_eq!(commits.len(), 1);
    let names: Vec<&str> = commits[0].files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["main.db", "writer.db"]);
    for f in &commits[0].files {
        assert_eq!(f.page_size, 4096);
        assert!(f.pages.contains_key(&1));
        assert_eq!(
            f.size as usize,
            f.pages.len(),
            "a new file writes every page"
        );
    }
}

#[test]
fn reads_of_one_store_do_not_commit_anything() {
    let volume = Volume::open(MemoryStorage::new()).unwrap();
    let conn = connect(&volume).unwrap();
    // The first write transaction on the empty writer.db gives it a header.
    conn.execute_batch("CREATE TABLE t (x); BEGIN IMMEDIATE; COMMIT;")
        .unwrap();
    let seq = volume.last_commit();
    for _ in 0..10 {
        let _: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
    }
    conn.execute_batch("BEGIN IMMEDIATE; COMMIT;").unwrap();
    assert_eq!(volume.last_commit(), seq);
}

#[test]
fn vacuum_shrinks_the_file() {
    let storage = MemoryStorage::new();
    let volume = Volume::open(storage.clone()).unwrap();
    let conn = connect(&volume).unwrap();
    conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY, v BLOB)")
        .unwrap();
    fill(&conn, 0, 300, 2000);
    let big = storage.file("main.db").unwrap().len();
    conn.execute_batch("DELETE FROM kv WHERE k >= 10; VACUUM;")
        .unwrap();
    let small = storage.file("main.db").unwrap().len();
    assert!(small < big / 4, "{small} should be much less than {big}");
    assert_eq!(integrity(&conn), "ok");
    drop(conn);
    drop(volume);

    let volume = Volume::open(storage).unwrap();
    let conn = connect(&volume).unwrap();
    assert_eq!(count(&conn, "kv"), 10);
    assert_eq!(integrity(&conn), "ok");
}

#[test]
fn export_is_a_plain_sqlite_file_and_import_replaces_it() {
    let volume = Volume::open(MemoryStorage::new()).unwrap();
    let conn = connect(&volume).unwrap();
    conn.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('one');")
        .unwrap();
    let exported = volume.export("main.db").unwrap();

    // Any SQLite can read the export.
    let dir = temp_dir("export");
    let path = dir.join("exported.db");
    std::fs::write(&path, &exported).unwrap();
    let plain = Connection::open(&path).unwrap();
    let body: String = plain
        .query_row("SELECT body FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "one");
    plain
        .execute_batch("INSERT INTO notes VALUES ('two'), ('three')")
        .unwrap();
    drop(plain);
    let edited = std::fs::read(&path).unwrap();

    // Import it while the connection stays open: the bumped change counter
    // makes SQLite drop its cache.
    let seq = volume.import("main.db", &edited).unwrap();
    assert_eq!(volume.last_commit(), seq);
    assert_eq!(count(&conn, "notes"), 3);
    assert_eq!(integrity(&conn), "ok");
    conn.execute_batch("INSERT INTO notes VALUES ('four')")
        .unwrap();
    assert_eq!(count(&conn, "notes"), 4);

    assert!(volume.import("main.db", b"not a database").is_err());
    assert_eq!(count(&conn, "notes"), 4);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn import_waits_for_an_open_read_transaction() {
    let volume = Volume::open(MemoryStorage::new()).unwrap();
    let conn = connect(&volume).unwrap();
    conn.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
        .unwrap();
    let file = volume.export("main.db").unwrap();
    conn.execute_batch("BEGIN; SELECT count(*) FROM t;")
        .unwrap();
    let err = volume.import("main.db", &file).unwrap_err();
    assert_eq!(err.kind(), moul_vfs::ErrorKind::Busy);
    conn.execute_batch("COMMIT").unwrap();
    volume.import("main.db", &file).unwrap();
    assert_eq!(count(&conn, "t"), 1);
}

#[test]
fn import_from_a_wal_mode_file_is_switched_to_rollback() {
    let dir = temp_dir("wal");
    let path = dir.join("wal.db");
    let src = Connection::open(&path).unwrap();
    src.execute_batch(
        "PRAGMA journal_mode = WAL; CREATE TABLE t (x); INSERT INTO t VALUES (1);
         PRAGMA wal_checkpoint(TRUNCATE);",
    )
    .unwrap();
    drop(src);
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes[18], 2, "the source says WAL");

    let volume = Volume::open(MemoryStorage::new()).unwrap();
    volume.import("main.db", &bytes).unwrap();
    let conn = connect(&volume).unwrap();
    assert_eq!(count(&conn, "t"), 1);
    let mode: String = conn
        .query_row("PRAGMA main.journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "memory");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn wal_mode_is_refused() {
    let volume = Volume::open(MemoryStorage::new()).unwrap();
    let conn = connect(&volume).unwrap();
    conn.execute_batch("CREATE TABLE t (x)").unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_ne!(mode, "wal");
    conn.execute_batch("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(count(&conn, "t"), 1);
}

#[test]
fn one_connection_per_volume() {
    let volume = Volume::open(MemoryStorage::new()).unwrap();
    let _conn = connect(&volume).unwrap();
    let second = connect(&volume);
    assert!(second.is_err(), "a second connection to main.db is refused");
}

#[test]
fn a_failed_copy_into_the_file_is_retried_and_reads_stay_correct() {
    let storage = MemoryStorage::new();
    let volume = Volume::open(storage.clone()).unwrap();
    let conn = connect(&volume).unwrap();
    conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY, v BLOB)")
        .unwrap();

    // Find a step inside the copy: after the log flush of the next commit.
    let probe = storage.fork();
    let pv = Volume::open(probe.clone()).unwrap();
    let pc = connect(&pv).unwrap();
    let start = probe.steps();
    fill(&pc, 0, 50, 500);
    let total = probe.steps() - start;
    drop(pc);
    drop(pv);
    // Steps: log write, log flush, then page writes. Fail the first page write.
    let at = storage.steps() + 2;
    assert!(total > 2);
    storage.fail_at(Some(at));
    fill(&conn, 0, 50, 500);
    // The commit is durable, so it succeeded; the pages sit in memory.
    assert_eq!(count(&conn, "kv"), 50);
    assert!(volume.checkpoint().is_err());
    storage.fail_at(None);
    volume.checkpoint().unwrap();
    assert_eq!(storage.file(LOG_NAME).unwrap().len(), 128);
    drop(conn);
    drop(volume);

    let volume = Volume::open(storage).unwrap();
    let conn = connect(&volume).unwrap();
    assert_eq!(count(&conn, "kv"), 50);
    assert_eq!(integrity(&conn), "ok");
}

#[test]
fn disk_storage_round_trip() {
    let dir = temp_dir("disk");
    {
        let volume = Volume::open(DiskStorage::new(&dir).unwrap()).unwrap();
        let conn = connect(&volume).unwrap();
        conn.execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY, v BLOB)")
            .unwrap();
        fill(&conn, 0, 1000, 100);
    }
    // The file on disk is a normal SQLite database.
    let plain = Connection::open(dir.join("main.db")).unwrap();
    assert_eq!(count(&plain, "kv"), 1000);
    drop(plain);

    let volume = Volume::open(DiskStorage::new(&dir).unwrap()).unwrap();
    let conn = connect(&volume).unwrap();
    assert_eq!(count(&conn, "kv"), 1000);
    assert_eq!(integrity(&conn), "ok");
    drop(conn);
    drop(volume);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn names_with_path_separators_are_refused() {
    let storage = MemoryStorage::new();
    assert!(storage.open("../escape").is_err());
    let volume = Volume::open(storage).unwrap();
    assert!(volume.export("a/b").is_err());
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "moul-vfs-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
