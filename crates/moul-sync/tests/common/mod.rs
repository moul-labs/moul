//! Devices that sync through one replica, for the scenario tests.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use moul_store::Store;
use moul_sync::{
    Local, Options, Prefixed, Replica, Retention, Role, Status, StoreLocal, Sync, WriterOnlyLocal,
    block_on,
};
use moul_vfs::{Capture, CrashMode, Fanout, MemoryStorage, Volume};

/// A clock every device and the replica share, moved by hand.
#[derive(Clone, Debug)]
pub struct Clock(Arc<AtomicI64>);

impl Clock {
    pub fn new() -> Self {
        Clock(Arc::new(AtomicI64::new(1_763_000_000_000)))
    }
    pub fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

/// A scratch folder under `target/`, emptied first.
pub fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("moul-sync")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One device: its stores on memory storage, the captures of its shared
/// and writer-only stores, and a sync for each. The writer-only store's
/// sync only pushes, as a server's does.
pub struct Device {
    pub name: String,
    pub storage: MemoryStorage,
    pub ltx: MemoryStorage,
    pub writer_ltx: MemoryStorage,
    pub store: Option<Store>,
    pub capture: Capture,
    pub writer_capture: Capture,
    pub sync: Sync,
    pub writer_sync: Sync,
    pub conflicts: PathBuf,
    replica: Box<dyn Fn() -> Box<dyn Replica>>,
    clock: Clock,
    retention: Retention,
}

fn open(storage: &MemoryStorage, ltx: &MemoryStorage, writer_ltx: &MemoryStorage) -> Opened {
    let capture = Capture::open(ltx.clone(), "main.db").unwrap();
    let writer_capture = Capture::open(writer_ltx.clone(), "writer.db").unwrap();
    let observers = Fanout::new()
        .with(capture.clone())
        .with(writer_capture.clone());
    let volume = Volume::open_observed(storage.clone(), observers).unwrap();
    (
        capture,
        writer_capture,
        Store::open_follower(volume).unwrap(),
    )
}

type Opened = (Capture, Capture, Store);

impl Device {
    pub fn new(
        name: &str,
        replica: impl Fn() -> Box<dyn Replica> + 'static,
        clock: &Clock,
        retention: Retention,
    ) -> Device {
        let storage = MemoryStorage::new();
        let ltx = MemoryStorage::new();
        let writer_ltx = MemoryStorage::new();
        let (capture, writer_capture, store) = open(&storage, &ltx, &writer_ltx);
        let conflicts = scratch(&format!("conflicts-{name}-{}", moul_sync::new_id()));
        let mut d = Device {
            name: name.to_owned(),
            storage,
            ltx,
            writer_ltx,
            store: Some(store),
            capture,
            writer_capture,
            sync: Sync::new(NoReplica, Options::default()),
            writer_sync: Sync::new(NoReplica, Options::default()),
            conflicts,
            replica: Box::new(replica),
            clock: clock.clone(),
            retention,
        };
        d.sync = d.new_sync();
        d.writer_sync = d.new_writer_sync();
        d
    }

    fn options(&self, push_only: bool) -> Options {
        let clock = self.clock.clone();
        Options {
            label: self.name.clone(),
            poll_every: 0,
            maintain_every: 0,
            retention: self.retention.clone(),
            follow_only: false,
            push_only,
            clock: Box::new(move || clock.now()),
        }
    }

    fn new_sync(&self) -> Sync {
        Sync::new((self.replica)(), self.options(false))
    }

    fn new_writer_sync(&self) -> Sync {
        Sync::new(Prefixed::writer_only((self.replica)()), self.options(true))
    }

    /// The writer-only store's part of the replica, as a replica of its own.
    pub fn writer_only_replica(&self) -> Prefixed<Box<dyn Replica>> {
        Prefixed::writer_only((self.replica)())
    }

    pub fn store(&self) -> &Store {
        self.store.as_ref().expect("open")
    }

    fn parts(&mut self) -> (&mut Sync, StoreLocal<'_>) {
        let local = StoreLocal {
            store: self.store.as_mut().expect("open"),
            capture: self.capture.clone(),
            conflicts: Some(self.conflicts.clone()),
        };
        (&mut self.sync, local)
    }

    fn writer_parts(&mut self) -> (&mut Sync, WriterOnlyLocal<'_>) {
        let local = WriterOnlyLocal {
            store: self.store.as_mut().expect("open"),
            capture: self.writer_capture.clone(),
            conflicts: Some(self.conflicts.clone()),
        };
        (&mut self.writer_sync, local)
    }

    /// One round of the writer-only store's sync, which only pushes.
    pub fn push_writer_only(&mut self) -> moul_sync::Result<()> {
        let (sync, mut local) = self.writer_parts();
        block_on(sync.sync(&mut local))
    }

    /// Takes the writer-only store over as a device that takes over writing
    /// does: with a sync that may follow, which fetches the store first.
    pub fn take_over_writer_only(&mut self) -> moul_sync::Result<()> {
        let mut sync = Sync::new(Prefixed::writer_only((self.replica)()), self.options(false));
        let (_, mut local) = self.writer_parts();
        block_on(sync.take_over(&mut local))
    }

    /// The writer-only store as a SQLite file.
    pub fn writer_db(&self) -> Vec<u8> {
        self.store().volume().export("writer.db").unwrap()
    }

    pub fn connect(&mut self) -> moul_sync::Result<()> {
        let (sync, mut local) = self.parts();
        block_on(sync.connect(&mut local))
    }

    pub fn sync(&mut self) -> moul_sync::Result<()> {
        let (sync, mut local) = self.parts();
        block_on(sync.sync(&mut local))
    }

    pub fn take_over(&mut self) -> moul_sync::Result<()> {
        let (sync, mut local) = self.parts();
        block_on(sync.take_over(&mut local))
    }

    pub fn disconnect(&mut self) -> moul_sync::Result<()> {
        let (sync, mut local) = self.parts();
        sync.disconnect(&mut local)
    }

    pub fn status(&self) -> Status {
        self.sync.status(&self.capture)
    }

    pub fn role(&self) -> Role {
        self.sync.role()
    }

    pub fn exec(&self, sql: &str) {
        self.store().conn().execute_batch(sql).unwrap();
    }

    pub fn try_exec(&self, sql: &str) -> rusqlite::Result<()> {
        self.store().conn().execute_batch(sql)
    }

    /// Every row of `t`, as text.
    pub fn rows(&self) -> Vec<String> {
        let conn = self.store().conn();
        let mut stmt = conn.prepare("SELECT v FROM t ORDER BY rowid").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    pub fn main_db(&self) -> Vec<u8> {
        self.store().export().unwrap()
    }

    pub fn integrity(&self) {
        let ok: String = self
            .store()
            .conn()
            .query_row("PRAGMA main.integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok", "{}", self.name);
    }

    /// Conflict copies kept, by file name.
    pub fn conflict_copies(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(&self.conflicts)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Closes everything and opens it again, as after the app restarts.
    pub fn restart(&mut self) {
        drop(self.store.take());
        self.reopen();
    }

    fn reopen(&mut self) {
        let (capture, writer_capture, store) = open(&self.storage, &self.ltx, &self.writer_ltx);
        self.capture = capture;
        self.writer_capture = writer_capture;
        self.store = Some(store);
        self.sync = self.new_sync();
        self.writer_sync = self.new_writer_sync();
    }

    /// Power loss: unflushed writes are handled by `mode`, then reopen.
    pub fn crash(&mut self, mode: CrashMode) {
        if let Some(store) = self.store.take() {
            std::mem::forget(store); // its handles are dead after the crash
        }
        self.storage.crash(mode);
        self.ltx.crash(mode);
        self.writer_ltx.crash(mode);
        self.reopen();
    }

    pub fn load_state(&mut self) -> Option<moul_sync::DeviceState> {
        let (_, mut local) = self.parts();
        local.load_state().unwrap()
    }
}

/// Stands in until the real replica is set.
struct NoReplica;
impl Replica for NoReplica {
    fn describe(&self) -> String {
        "none".into()
    }
    fn get<'a>(
        &'a self,
        _: &'a str,
    ) -> moul_sync::BoxFuture<'a, moul_sync::Result<Option<moul_sync::Object>>> {
        unreachable!()
    }
    fn put<'a>(
        &'a self,
        _: &'a str,
        _: &'a [u8],
        _: moul_sync::Condition,
    ) -> moul_sync::BoxFuture<'a, moul_sync::Result<moul_sync::Put>> {
        unreachable!()
    }
    fn list<'a>(
        &'a self,
        _: &'a str,
    ) -> moul_sync::BoxFuture<'a, moul_sync::Result<Vec<moul_sync::ObjectInfo>>> {
        unreachable!()
    }
    fn delete<'a>(&'a self, _: &'a str) -> moul_sync::BoxFuture<'a, moul_sync::Result<()>> {
        unreachable!()
    }
}

/// Asserts that every device holds byte for byte the same shared store,
/// and that it is what the replica restores to.
pub fn assert_converged(replica: &dyn Replica, devices: &[&Device]) {
    let first = devices[0].main_db();
    for d in devices {
        d.integrity();
        assert!(
            d.main_db() == first,
            "{} differs from {} ({} vs {} bytes)",
            d.name,
            devices[0].name,
            d.main_db().len(),
            first.len()
        );
    }
    let (_, restored) = block_on(moul_sync::restore::restore_db(replica, None)).unwrap();
    assert!(restored == first, "the replica restores to something else");
}
