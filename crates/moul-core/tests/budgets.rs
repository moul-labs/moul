//! Time budgets, measured natively: opening the engine on a new database
//! and on one with a few thousand records, and a commit (one record created
//! through `Engine::handle`). The engine opens on the browser host's storage
//! stack, a volume whose commits capture writes as level-0 files beside the
//! database, so a commit costs what it costs in a tab. (The host also first
//! opens the files once as a follower, to read the device's saved state;
//! that step is not timed here.)
//!
//! Each is measured twice: on a folder on disk, where every commit waits for
//! the disk to flush (on macOS `F_FULLFSYNC`, which is most of the time), and
//! on memory storage, which leaves the engine's own work: SQLite, rules,
//! capture's LTX encoding, the change log.
//!
//! Ignored by default, because timings mean little in a debug build. CI runs
//! them with:
//!
//! ```sh
//! cargo test -p moul-core --release --test budgets -- --ignored --nocapture
//! ```
//!
//! The limits are about five times what an Apple M-series laptop measured
//! (noted beside each), so they catch a regression, not noise or a slower
//! runner.

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use moul_core::{Config, Engine, MemoryBlobs, Request, Response, SystemHost};
use moul_store::Store;
use moul_vfs::{Capture, DiskStorage, MemoryStorage, Storage, Volume};
use serde_json::{Value, json};

/// The budgets of one kind of storage.
struct Budgets {
    /// Opening an engine on a new database: creating the stores and migrating.
    open_new: Duration,
    /// Opening an engine on a database that holds a few thousand records.
    open_full: Duration,
    /// The median time of one record created through `Engine::handle`.
    commit: Duration,
}

/// On disk. Measured: open new 175 ms, open full 1.1 ms, commit 36 ms, most
/// of it `F_FULLFSYNC` (Linux's `fsync` is quicker).
const DISK: Budgets = Budgets {
    open_new: Duration::from_millis(1_000),
    open_full: Duration::from_millis(10),
    commit: Duration::from_millis(200),
};

/// In memory, the engine's own cost. Measured: open new 1.8 ms, open full
/// 0.6 ms, commit 0.42 ms.
const MEMORY: Budgets = Budgets {
    open_new: Duration::from_millis(10),
    open_full: Duration::from_millis(5),
    commit: Duration::from_millis(2),
};

const RECORDS: usize = 5_000;
const COMMITS: usize = 400;

/// Where the databases on disk go, under `target/`.
fn scratch_root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("moul-core-budgets")
}

/// A scratch folder under `target/`, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = scratch_root().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Where a database lives: its volume's storage and capture's.
trait Place {
    fn storages(&self) -> (impl Storage + 'static, impl Storage + 'static);
}

/// A folder on disk, as the native host keeps a database: the volume in the
/// folder, capture's files in `ltx/` (OPFS has the same layout).
struct Disk(PathBuf);

impl Place for Disk {
    fn storages(&self) -> (impl Storage + 'static, impl Storage + 'static) {
        (
            DiskStorage::new(&self.0).unwrap(),
            DiskStorage::new(self.0.join("ltx")).unwrap(),
        )
    }
}

/// Memory storage that outlives the engine, so a database can be reopened.
#[derive(Default)]
struct Memory(MemoryStorage, MemoryStorage);

impl Place for Memory {
    fn storages(&self) -> (impl Storage + 'static, impl Storage + 'static) {
        (self.0.clone(), self.1.clone())
    }
}

/// Opens the engine on the database at `place`, on the storage stack the
/// browser host uses on OPFS.
fn open(place: &impl Place) -> Engine {
    let (storage, ltx) = place.storages();
    let capture = Capture::open(ltx, "main.db").unwrap();
    let volume = Volume::open_observed(storage, capture).unwrap();
    let store = Store::open(volume).unwrap();
    Engine::open(store, SystemHost, MemoryBlobs::new(), Config::default()).unwrap()
}

fn ok(r: Response, status: u16) -> Value {
    assert_eq!(r.status, status, "{:?}", r.body);
    r.json_body().clone()
}

fn define_notes(e: &mut Engine) {
    ok(
        e.handle(
            Request::post(
                "/api/moul",
                json!({
                    "name": "notes",
                    "fields": [
                        {"name": "title", "type": "text", "required": true},
                        {"name": "body", "type": "text"},
                        {"name": "views", "type": "number"},
                        {"name": "done", "type": "bool"},
                    ],
                }),
            )
            .owner(),
        ),
        201,
    );
}

fn note(i: usize) -> Value {
    json!({
        "title": format!("Note {i}"),
        "body": format!("Something to remember, number {i}, written down so it is not forgotten."),
        "views": i % 97,
        "done": i.is_multiple_of(3),
    })
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

fn report(what: &str, got: Duration, limit: Duration) -> bool {
    let pass = got <= limit;
    println!(
        "  {} {what}: {} (budget {})",
        if pass { "ok  " } else { "OVER" },
        ms(got),
        ms(limit)
    );
    pass
}

/// Measures `place`s made by `new`, reports, and returns whether every
/// budget held.
fn measure<P: Place>(what: &str, new: impl Fn(&str) -> P, budgets: &Budgets) -> bool {
    // A new database, opened five times, each in a place of its own.
    let open_new = median(
        (0..5)
            .map(|i| {
                let place = new(&format!("new-{i}"));
                let started = Instant::now();
                let e = open(&place);
                let took = started.elapsed();
                drop(e);
                took
            })
            .collect(),
    );

    // A database with a few thousand records, closed and opened again.
    let place = new("full");
    {
        let mut e = open(&place);
        define_notes(&mut e);
        for chunk in (0..RECORDS).collect::<Vec<_>>().chunks(1_000) {
            let rows: Vec<Value> = chunk.iter().map(|&i| note(i)).collect();
            ok(
                e.handle(Request::post("/api/moul/notes/import", json!(rows)).owner()),
                200,
            );
        }
    }
    let open_full = median(
        (0..5)
            .map(|_| {
                let started = Instant::now();
                let e = open(&place);
                let took = started.elapsed();
                drop(e);
                took
            })
            .collect(),
    );

    // Commits: one record at a time, through a request, on that database.
    let mut e = open(&place);
    let mut commits = Vec::with_capacity(COMMITS);
    for i in 0..COMMITS {
        let req = Request::post("/api/moul/notes/records", note(RECORDS + i)).owner();
        let started = Instant::now();
        let r = e.handle(req);
        commits.push(started.elapsed());
        ok(r, 201);
    }
    let total = ok(
        e.handle(
            Request::get("/api/moul/notes/records")
                .query("perPage", "1")
                .owner(),
        ),
        200,
    );
    assert_eq!(total["totalItems"], json!(RECORDS + COMMITS));
    let commit = median(commits);
    drop(e);

    println!("{what}:");
    let results = [
        report("open a new database", open_new, budgets.open_new),
        report(
            &format!("open a database of {RECORDS} records"),
            open_full,
            budgets.open_full,
        ),
        report(
            &format!("commit one record (median of {COMMITS})"),
            commit,
            budgets.commit,
        ),
    ];
    results.iter().all(|&p| p)
}

#[test]
#[ignore = "a timing budget: run with --release --ignored"]
fn engine_time_budgets() {
    let disk = measure("on disk", |name| Disk(scratch(name)), &DISK);
    let memory = measure("in memory", |_| Memory::default(), &MEMORY);
    let _ = std::fs::remove_dir_all(scratch_root());
    assert!(disk && memory, "a time budget was exceeded");
}
