//! The time budget of restoring a 50 MB database from a replica: listing a
//! folder replica, reading its files, decoding them and merging them into a
//! database file with `moul_ltx::restore`, as a new device does when it joins
//! (`moul_sync::restore::restore_db`).
//!
//! The replica is made the way capture and maintenance make one: a database
//! grown to about 50 MB in commits of about 1 MB, each commit's level-0 file,
//! and a snapshot (level 9) of the database a few commits before the end, as
//! maintenance writes it. A restore takes the snapshot and the level-0 files
//! after it.
//!
//! Ignored by default, because timings mean little in a debug build. CI runs
//! it with:
//!
//! ```sh
//! cargo test -p moul-sync --release --test budgets -- --ignored --nocapture
//! ```

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use moul_ltx::{Header, SNAPSHOT_LEVEL, Txid, encode, lock_pgno};
use moul_sync::layout::ltx_key;
use moul_sync::{Condition, DirReplica, Put, Replica, block_on};
use moul_vfs::{Capture, MemoryStorage, Volume};
use rusqlite::{Connection, OpenFlags, params};

/// The size the database is grown to.
const TARGET_BYTES: u64 = 50 * 1024 * 1024;
/// Commits after the snapshot, each a level-0 file the restore applies.
const AFTER_SNAPSHOT: usize = 8;
/// About five times what an Apple M-series laptop measured (426 ms).
const RESTORE: Duration = Duration::from_millis(2_000);

/// A scratch folder under `target/`, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("moul-sync-budgets")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Text that reads like notes, from a fixed seed, so LZ4 finds what it would
/// find in real records.
struct Words(u64);

impl Words {
    const LIST: [&'static str; 24] = [
        "the", "meeting", "moved", "to", "Thursday", "bring", "the", "draft", "and", "notes",
        "about", "budget", "review", "with", "Ana", "before", "lunch", "order", "more", "oat",
        "milk", "call", "plumber", "invoice",
    ];

    fn next(&mut self) -> u64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn sentence(&mut self, bytes: usize) -> String {
        let mut s = String::with_capacity(bytes + 16);
        while s.len() < bytes {
            let n = self.next();
            if n.is_multiple_of(5) {
                s.push_str(&format!("{} ", n % 100_000));
            } else {
                s.push_str(Self::LIST[(n % Self::LIST.len() as u64) as usize]);
                s.push(' ');
            }
        }
        s
    }
}

/// Puts `bytes` into `replica` under `key`.
fn put(replica: &DirReplica, key: &str, bytes: &[u8]) {
    let r = block_on(replica.put(key, bytes, Condition::None)).unwrap();
    assert!(matches!(r, Put::Written { .. }), "{key}: {r:?}");
}

/// The snapshot of `db` at `head`, as maintenance encodes it.
fn snapshot(db: &[u8], head: Txid) -> Vec<u8> {
    let raw = u16::from_be_bytes([db[16], db[17]]);
    let ps = if raw == 1 { 65_536 } else { raw as u32 };
    let pages = (db.len() / ps as usize) as u32;
    let lock = lock_pgno(ps);
    let header = Header::new(ps, pages, Txid(1), head, 1_763_000_000_000);
    encode(
        header,
        (1..=pages).filter(|&p| p != lock).map(|p| {
            let at = (p as usize - 1) * ps as usize;
            (p, &db[at..at + ps as usize])
        }),
    )
    .unwrap()
}

/// Grows a database to [`TARGET_BYTES`] on a captured volume and writes its
/// replica into `dir`. Returns the database as it ends.
fn make_replica(dir: &std::path::Path) -> Vec<u8> {
    let capture = Capture::open(MemoryStorage::new(), "main.db")
        .unwrap()
        .with_clock({
            let mut t = 1_763_000_000_000;
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
    conn.execute_batch(
        "PRAGMA journal_mode = MEMORY;
         PRAGMA synchronous = OFF;
         CREATE TABLE notes (id TEXT PRIMARY KEY, title TEXT NOT NULL, body TEXT, done INTEGER);
         CREATE INDEX notes_title ON notes (title);",
    )
    .unwrap();

    let size = |conn: &Connection| -> u64 {
        conn.query_row(
            "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap() as u64
    };
    let mut words = Words(0x9e37_79b9_7f4a_7c15);
    let mut n = 0u64;
    let mut commit = |conn: &Connection, words: &mut Words| {
        let tx = conn.unchecked_transaction().unwrap();
        for _ in 0..1_000 {
            n += 1;
            tx.execute(
                "INSERT INTO notes VALUES (?1, ?2, ?3, ?4)",
                params![
                    format!("note-{:012x}", words.next() >> 16),
                    words.sentence(40),
                    words.sentence(900),
                    n.is_multiple_of(3),
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    };
    // Grow to the target, keeping the last few commits for after the snapshot.
    let per_commit = 1_100 * 1_000;
    while size(&conn) + (AFTER_SNAPSHOT as u64) * per_commit < TARGET_BYTES {
        commit(&conn, &mut words);
    }
    let head = capture.position().txid;
    let snap = snapshot(&volume.export("main.db").unwrap(), head);
    for _ in 0..AFTER_SNAPSHOT {
        commit(&conn, &mut words);
    }
    let last = capture.position().txid;

    let replica = DirReplica::new(dir);
    put(&replica, &ltx_key(SNAPSHOT_LEVEL, Txid(1), head), &snap);
    for t in 1..=last.0 {
        let bytes = capture.read(Txid(t)).unwrap();
        put(&replica, &ltx_key(0, Txid(t), Txid(t)), &bytes);
    }
    drop(conn);
    volume.export("main.db").unwrap()
}

#[test]
#[ignore = "a timing budget: run with --release --ignored"]
fn restore_time_budget() {
    let dir = scratch("replica");
    let want = make_replica(&dir);
    let replica = DirReplica::new(&dir);

    let mut times = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let (header, db) = block_on(moul_sync::restore::restore_db(&replica, None)).unwrap();
        times.push(started.elapsed());
        assert_eq!(header.min_txid, Txid(1));
        assert!(
            db == want,
            "the restored database differs from the writer's"
        );
    }
    times.sort();
    let restore = times[times.len() / 2];

    let mb = want.len() as f64 / 1024.0 / 1024.0;
    let pass = restore <= RESTORE;
    println!(
        "  {} restore a {mb:.1} MB database (snapshot and {AFTER_SNAPSHOT} level-0 files, median of 3): {:.1} ms (budget {} ms)",
        if pass { "ok  " } else { "OVER" },
        restore.as_secs_f64() * 1000.0,
        RESTORE.as_millis(),
    );
    assert!(want.len() as u64 >= TARGET_BYTES, "{mb:.1} MB only");
    // The replica is over 100 MB: leave nothing behind.
    let _ = std::fs::remove_dir_all(&dir);
    assert!(pass, "the restore budget was exceeded");
}
