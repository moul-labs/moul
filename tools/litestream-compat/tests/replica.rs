//! Whole replicas, both ways: `litestream restore` rebuilds what Moul writes,
//! and Moul rebuilds and follows what Litestream writes. Databases are
//! compared byte for byte, so page by page.

use std::collections::BTreeMap;
use std::path::Path;

use litestream_compat::{harness_or_skip, list, moul_restore, put_file, read_file, scratch_dir};
use moul_ltx::{Ltx, SNAPSHOT_LEVEL, Txid, compact_to_vec, restore};
use moul_vfs::{Capture, MemoryStorage, Volume};
use rusqlite::{Connection, OpenFlags, params};

/// The database after each TXID of a Moul replica.
type States = BTreeMap<u64, Vec<u8>>;

/// Runs a workload on a captured volume with `page_size`, copies the level-0
/// files into `replica`, and returns the database at every TXID.
fn moul_replica(page_size: u32, replica: &Path) -> (Capture, States) {
    let capture = Capture::open(MemoryStorage::new(), "main.db")
        .unwrap()
        .with_clock({
            let mut t = 1_731_500_000_000;
            move || {
                t += 1000;
                t
            }
        });
    let volume = Volume::open_observed(MemoryStorage::new(), capture.clone()).unwrap();
    let conn = Connection::open_with_flags_and_vfs(
        "main.db",
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        volume.vfs_name(),
    )
    .unwrap();
    conn.execute_batch(&format!(
        "PRAGMA page_size = {page_size};
         PRAGMA journal_mode = MEMORY;
         PRAGMA synchronous = OFF;
         ATTACH 'writer.db' AS writer;
         PRAGMA writer.page_size = {page_size};"
    ))
    .unwrap();

    let mut states = States::new();
    let mut record = |what: &str| {
        let txid = capture.position().txid.0;
        let prev = states.keys().last().copied().unwrap_or(0);
        assert!(
            txid <= prev + 1,
            "{what} wrote {} TXIDs; record each",
            txid - prev
        );
        let db = volume.export("main.db").unwrap();
        if let Some(prev) = states.insert(txid, db.clone()) {
            assert_eq!(prev, db, "{what} changed main.db without a new TXID");
        }
    };

    conn.execute_batch(
        "BEGIN;
         CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT, body BLOB);
         CREATE INDEX notes_title ON notes (title);
         COMMIT;",
    )
    .unwrap();
    record("create");
    for i in 0..40 {
        conn.execute(
            "INSERT INTO notes (title, body) VALUES (?1, ?2)",
            params![format!("note {i}"), vec![i as u8; 300 + 97 * i]],
        )
        .unwrap();
        record("insert");
    }
    conn.execute_batch("CREATE TABLE writer.secrets (k TEXT PRIMARY KEY, v TEXT)")
        .unwrap();
    record("writer only");
    conn.execute_batch(
        "BEGIN;
         UPDATE notes SET title = upper(title) WHERE id % 3 = 0;
         INSERT INTO secrets VALUES ('key', 'value');
         COMMIT;",
    )
    .unwrap();
    record("across stores");
    // An overflow chain, then most of the table gone and the file shrunk.
    conn.execute(
        "INSERT INTO notes (title, body) VALUES ('big', ?1)",
        params![vec![7u8; 200_000]],
    )
    .unwrap();
    record("big");
    conn.execute_batch("DELETE FROM notes WHERE id > 5")
        .unwrap();
    record("delete");
    conn.execute_batch("VACUUM").unwrap();
    record("vacuum");
    for i in 0..5 {
        conn.execute(
            "INSERT INTO notes (title) VALUES (?1)",
            params![format!("after {i}")],
        )
        .unwrap();
        record("after vacuum");
    }

    let last = capture.position().txid.0;
    assert!(last > 45, "only {last} TXIDs");
    for t in 1..=last {
        put_file(replica, 0, &capture.read(Txid(t)).unwrap());
    }
    (capture, states)
}

fn assert_same(got: &[u8], want: &[u8], what: &str) {
    assert!(
        got.len() == want.len(),
        "{what}: {} bytes, expected {}",
        got.len(),
        want.len()
    );
    let pages = got.chunks(512).zip(want.chunks(512));
    if let Some(i) = pages.clone().position(|(a, b)| a != b) {
        panic!(
            "{what}: first difference in the 512-byte block at {}",
            i * 512
        );
    }
}

#[test]
fn litestream_restores_level_zero_files_moul_wrote() {
    let go = harness_or_skip!();
    for page_size in [512, 4096, 65_536] {
        let replica = scratch_dir(&format!("moul-l0-{page_size}"));
        let (_, states) = moul_replica(page_size, &replica);
        let (&last, newest) = states.last_key_value().unwrap();

        let restored = go.restore(&replica, None).unwrap();
        assert_same(&restored, newest, &format!("page size {page_size}, latest"));
        for t in [1, 2, last / 2, last - 1] {
            let restored = go.restore(&replica, Some(Txid(t))).unwrap();
            assert_same(
                &restored,
                &states[&t],
                &format!("page size {page_size}, TXID {t}"),
            );
        }
        assert_same(
            &moul_restore(&replica, None).unwrap(),
            newest,
            "Moul's own restore",
        );
    }
}

#[test]
fn litestream_restores_compactions_and_snapshots_moul_wrote() {
    let go = harness_or_skip!();
    let replica = scratch_dir("moul-levels");
    let (_, states) = moul_replica(4096, &replica);
    let last = *states.keys().last().unwrap();
    let l0: Vec<Vec<u8>> = (1..=last)
        .map(|t| read_file(&replica, &list(&replica)[t as usize - 1]))
        .collect();
    let decoded: Vec<Ltx<'_>> = l0.iter().map(|b| Ltx::decode(b).unwrap()).collect();

    // Level 1 in three ranges, level 2 over the first two, and a snapshot
    // part way, as Moul's compaction will lay them out.
    let (a, b) = (last as usize / 3, 2 * last as usize / 3);
    for range in [0..a, a..b, b..last as usize] {
        put_file(&replica, 1, &compact_to_vec(&decoded[range]).unwrap());
    }
    put_file(&replica, 2, &compact_to_vec(&decoded[..b]).unwrap());
    put_file(
        &replica,
        SNAPSHOT_LEVEL,
        &compact_to_vec(&decoded[..a + 3]).unwrap(),
    );

    let (_, newest) = states.last_key_value().unwrap();
    assert_same(&go.restore(&replica, None).unwrap(), newest, "latest");
    for t in [a as u64, a as u64 + 3, b as u64, last - 1] {
        assert_same(
            &go.restore(&replica, Some(Txid(t))).unwrap(),
            &states[&t],
            &format!("TXID {t}"),
        );
        assert_same(
            &moul_restore(&replica, Some(Txid(t))).unwrap(),
            &states[&t],
            &format!("Moul, TXID {t}"),
        );
    }

    // Without the level-0 files, the levels above still reach the end.
    std::fs::remove_dir_all(replica.join("ltx/0")).unwrap();
    assert_same(&go.restore(&replica, None).unwrap(), newest, "no level 0");
    assert_same(
        &moul_restore(&replica, None).unwrap(),
        newest,
        "Moul, no level 0",
    );
}

#[test]
fn litestream_compacts_a_replica_moul_wrote() {
    let go = harness_or_skip!();
    let replica = scratch_dir("moul-compacted-by-litestream");
    let (_, states) = moul_replica(4096, &replica);
    go.compact_replica(&replica, 1).unwrap();
    go.compact_replica(&replica, 2).unwrap();
    assert!(list(&replica).iter().any(|f| f.level == 2));

    std::fs::remove_dir_all(replica.join("ltx/0")).unwrap();
    std::fs::remove_dir_all(replica.join("ltx/1")).unwrap();
    let (_, newest) = states.last_key_value().unwrap();
    assert_same(&go.restore(&replica, None).unwrap(), newest, "Litestream");
    assert_same(&moul_restore(&replica, None).unwrap(), newest, "Moul");
}

/// A script for the harness: SQL, with Litestream syncing after each step,
/// compacting and snapshotting along the way.
fn litestream_script(page_size: u32) -> String {
    let mut s = format!(
        "PRAGMA page_size = {page_size};
PRAGMA journal_mode = WAL;
CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, data BLOB);
CREATE INDEX items_name ON items (name);
-- sync
"
    );
    for i in 0..30 {
        s += &format!(
            "INSERT INTO items (name, data) VALUES ('item {i}', randomblob({}));\n-- sync\n",
            200 + 61 * i
        );
        if i == 10 {
            s += "-- compact 1\n";
        }
        if i == 15 {
            s += "-- snapshot\n";
        }
    }
    s += "UPDATE items SET name = upper(name) WHERE id % 2 = 0;\n-- sync\n";
    s += "INSERT INTO items (name, data) VALUES ('big', randomblob(150000));\n-- sync\n";
    s += "-- compact 1\n-- compact 2\n";
    s += "DELETE FROM items WHERE id > 8;\n-- sync\n-- checkpoint\n";
    s += "VACUUM;\n-- sync\n-- checkpoint\n";
    for i in 0..4 {
        s += &format!("INSERT INTO items (name) VALUES ('late {i}');\n-- sync\n");
    }
    s += "-- compact 1\n";
    s += "INSERT INTO items (name) VALUES ('last');\n-- sync\n";
    s
}

fn count_items(db: &[u8]) -> i64 {
    let dir = scratch_dir("open");
    let path = dir.join("check.db");
    std::fs::write(&path, db).unwrap();
    let conn = Connection::open(&path).unwrap();
    let ok: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ok, "ok");
    conn.query_row("SELECT count(*) FROM items", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn moul_restores_a_replica_litestream_wrote() {
    let go = harness_or_skip!();
    for page_size in [512, 4096, 65_536] {
        let dir = scratch_dir(&format!("litestream-{page_size}"));
        let replica = dir.join("replica");
        go.replicate(
            &dir.join("source.db"),
            &replica,
            &litestream_script(page_size),
        )
        .unwrap_or_else(|e| panic!("page size {page_size}: {e}"));
        let files = list(&replica);
        for level in [0, 1, 2, SNAPSHOT_LEVEL] {
            assert!(
                files.iter().any(|f| f.level == level),
                "no level {level} files"
            );
        }

        let theirs = go.restore(&replica, None).unwrap();
        let ours = moul_restore(&replica, None).unwrap();
        assert_same(&ours, &theirs, &format!("page size {page_size}, latest"));
        assert_eq!(count_items(&ours), 13);

        // Every TXID a level-0 file ends at.
        for f in files.iter().filter(|f| f.level == 0) {
            let t = f.max_txid;
            assert_same(
                &moul_restore(&replica, Some(t)).unwrap(),
                &go.restore(&replica, Some(t)).unwrap(),
                &format!("page size {page_size}, TXID {t}"),
            );
        }
    }
}

#[test]
fn moul_follows_a_replica_litestream_wrote() {
    let go = harness_or_skip!();
    let dir = scratch_dir("litestream-follow");
    let replica = dir.join("replica");
    go.replicate(&dir.join("source.db"), &replica, &litestream_script(4096))
        .unwrap();

    // Join from the snapshot, then apply each level-0 file after it in turn.
    let files = list(&replica);
    let snapshot = files.iter().find(|f| f.level == SNAPSHOT_LEVEL).unwrap();
    let bytes = read_file(&replica, snapshot);
    let mut db = restore::restore_to_vec(&[Ltx::decode(&bytes).unwrap()]).unwrap();
    let mut position = snapshot.max_txid;
    let after: Vec<_> = files
        .iter()
        .filter(|f| f.level == 0 && f.max_txid > snapshot.max_txid)
        .collect();
    assert!(!after.is_empty());
    for f in after {
        let bytes = read_file(&replica, f);
        position = restore::apply(&mut db, position, &Ltx::decode(&bytes).unwrap()).unwrap();
        assert_same(
            &db,
            &go.restore(&replica, Some(position)).unwrap(),
            &format!("TXID {position}"),
        );
    }
    assert_same(
        &db,
        &go.restore(&replica, None).unwrap(),
        "followed to the end",
    );
}

/// Rewrites `fixtures/litestream`, a small replica Litestream wrote, which
/// `moul-ltx` restores in its own tests without Go:
///
/// ```sh
/// cargo test -p litestream-compat --test replica -- --ignored regenerate_fixture
/// ```
#[test]
#[ignore = "rewrites fixtures/litestream; run by hand"]
fn regenerate_fixture() {
    let go = harness_or_skip!();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/litestream");
    let replica = root.join("replica");
    let _ = std::fs::remove_dir_all(&replica);
    let dir = scratch_dir("fixture");
    go.replicate(&dir.join("source.db"), &replica, FIXTURE_SCRIPT)
        .unwrap();
    std::fs::write(
        root.join("expected.db"),
        go.restore(&replica, None).unwrap(),
    )
    .unwrap();
}

const FIXTURE_SCRIPT: &str = "PRAGMA page_size = 4096;
PRAGMA journal_mode = WAL;
CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT);
-- sync
INSERT INTO items (name) VALUES ('one'), ('two');
-- sync
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200)
INSERT INTO items (name) SELECT 'row ' || i FROM n;
-- sync
-- compact 1
-- snapshot
UPDATE items SET name = upper(name) WHERE id % 2 = 0;
-- sync
DELETE FROM items WHERE id > 50;
-- sync
-- checkpoint
VACUUM;
-- sync
INSERT INTO items (name) VALUES ('last');
-- sync
";

#[test]
fn the_committed_fixture_restores_the_same_with_litestream() {
    let go = harness_or_skip!();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/litestream");
    let expected = std::fs::read(root.join("expected.db")).unwrap();
    let replica = root.join("replica");
    assert_same(
        &go.restore(&replica, None).unwrap(),
        &expected,
        "Litestream",
    );
    assert_same(&moul_restore(&replica, None).unwrap(), &expected, "Moul");
}
