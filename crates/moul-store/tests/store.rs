//! The three-store layout, export and import.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use moul_store::{Error, FILES, Store, StoreKind};
use moul_vfs::{MemoryStorage, Storage, StorageFile, Volume};
use rusqlite::Connection;
use rusqlite::hooks::{AuthContext, Authorization};

fn store(storage: &MemoryStorage) -> Store {
    Store::open(Volume::open(storage.clone()).unwrap()).unwrap()
}

fn app_id(file: &[u8]) -> [u8; 4] {
    file[68..72].try_into().unwrap()
}

#[test]
fn a_new_database_has_three_marked_stores() {
    let storage = MemoryStorage::new();
    let s = store(&storage);
    for kind in StoreKind::ALL {
        let file = storage.file(kind.file_name()).expect("file exists");
        assert_eq!(&file[..16], b"SQLite format 3\0");
        assert_eq!(u16::from_be_bytes([file[16], file[17]]), 4096);
        assert_eq!(app_id(&file), kind.application_id().to_be_bytes());
    }
    let schemas: Vec<String> = s
        .conn()
        .prepare("SELECT name FROM pragma_database_list ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(schemas, ["main", "writer", "device"]);
    assert_eq!(FILES, ["main.db", "writer.db", "device.db"]);
    // Marking the three files was one commit.
    assert_eq!(s.volume().last_commit().0, 1);
}

#[test]
fn a_record_and_its_secret_commit_together() {
    let storage = MemoryStorage::new();
    let mut s = store(&storage);
    s.write(|tx| {
        tx.execute_batch(
            "CREATE TABLE main.users (id TEXT PRIMARY KEY, email TEXT);
             CREATE TABLE writer.passwords (user TEXT PRIMARY KEY, hash TEXT);
             CREATE TABLE device.drafts (id INTEGER PRIMARY KEY, body TEXT);",
        )
    })
    .unwrap();
    let before = s.volume().last_commit();
    let failed: Result<(), rusqlite::Error> = s.write(|tx| {
        tx.execute("INSERT INTO users VALUES ('u1', 'a@example.com')", [])?;
        tx.execute("INSERT INTO passwords VALUES ('u1', 'hash')", [])?;
        tx.execute("INSERT INTO nope VALUES (1)", [])?;
        Ok(())
    });
    assert!(failed.is_err());
    assert_eq!(s.volume().last_commit(), before, "nothing committed");

    s.write(|tx| {
        tx.execute("INSERT INTO users VALUES ('u1', 'a@example.com')", [])?;
        tx.execute("INSERT INTO passwords VALUES ('u1', 'hash')", [])
    })
    .unwrap();
    assert_eq!(
        s.volume().last_commit().0,
        before.0 + 1,
        "one commit for both stores"
    );
    drop(s);

    let s = store(&storage);
    let n: i64 = s
        .conn()
        .query_row(
            "SELECT count(*) FROM users JOIN passwords ON passwords.user = users.id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn the_export_holds_no_secrets() {
    let mut s = Store::open_in_memory().unwrap();
    s.write(|tx| {
        tx.execute_batch(
            "CREATE TABLE notes (body TEXT);
             INSERT INTO notes VALUES ('shared');
             CREATE TABLE writer.keys (k TEXT);
             INSERT INTO writer.keys VALUES ('very-secret-key');",
        )
    })
    .unwrap();
    let exported = s.export().unwrap();
    assert_eq!(app_id(&exported), *b"MOUL");
    let needle = b"very-secret-key";
    assert!(!exported.windows(needle.len()).any(|w| w == needle));
    assert!(exported.windows(6).any(|w| w == b"shared"));
}

#[test]
fn export_then_import_into_another_database() {
    let mut a = Store::open_in_memory().unwrap();
    a.write(|tx| {
        tx.execute_batch(
            "CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('from a');
             CREATE TABLE writer.keys (k TEXT); INSERT INTO writer.keys VALUES ('a-key');",
        )
    })
    .unwrap();
    let file = a.export().unwrap();

    let mut b = Store::open_in_memory().unwrap();
    b.write(|tx| {
        tx.execute_batch(
            "CREATE TABLE writer.keys (k TEXT); INSERT INTO writer.keys VALUES ('b-key');",
        )
    })
    .unwrap();
    b.import(&file).unwrap();
    let body: String = b
        .conn()
        .query_row("SELECT body FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "from a");
    let key: String = b
        .conn()
        .query_row("SELECT k FROM writer.keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(key, "b-key", "the writer-only store stays");
}

#[test]
fn a_plain_sqlite_file_can_be_imported() {
    let mut src = Store::open_in_memory().unwrap();
    src.write(|tx| tx.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (42);"))
        .unwrap();
    let mut file = src.export().unwrap();
    file[68..72].fill(0); // no application_id: any SQLite file

    let storage = MemoryStorage::new();
    let mut s = store(&storage);
    s.import(&file).unwrap();
    let x: i64 = s
        .conn()
        .query_row("SELECT x FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(x, 42);
    drop(s);
    assert_eq!(
        app_id(&storage.file("main.db").unwrap()),
        *b"MOUL",
        "adopted"
    );
}

#[test]
fn bad_imports_are_refused_and_change_nothing() {
    let mut s = Store::open_in_memory().unwrap();
    s.write(|tx| tx.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);"))
        .unwrap();
    let before = s.export().unwrap();
    let seq = s.volume().last_commit();

    let garbage = vec![7u8; 8192];
    assert!(matches!(s.import(&garbage), Err(Error::BadImport(_))));

    // A writer-only store must never become the shared one.
    let storage = MemoryStorage::new();
    drop(store(&storage));
    let writer = storage.file("writer.db").unwrap();
    let err = s.import(&writer).unwrap_err();
    assert!(err.to_string().contains("writer-only"), "{err}");

    // A damaged file fails the integrity check.
    let mut damaged = before.clone();
    let pages = damaged.len() / 4096;
    assert!(pages >= 2);
    for b in &mut damaged[4096 + 8..4096 + 64] {
        *b ^= 0x5A;
    }
    assert!(matches!(s.import(&damaged), Err(Error::BadImport(_))));

    // Another page size is refused with a way out.
    let mut big = before.clone();
    big[16..18].copy_from_slice(&8192u16.to_be_bytes());
    let err = s.import(&big).unwrap_err();
    assert!(err.to_string().contains("VACUUM"), "{err}");

    assert_eq!(s.volume().last_commit(), seq);
    assert_eq!(s.export().unwrap(), before);
}

fn notes_store() -> (MemoryStorage, Store) {
    let storage = MemoryStorage::new();
    let mut s = store(&storage);
    s.write(|tx| {
        tx.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('kept');")
    })
    .unwrap();
    (storage, s)
}

fn note(s: &Store) -> rusqlite::Result<String> {
    s.conn()
        .query_row("SELECT body FROM notes", [], |r| r.get(0))
}

/// Memory storage whose next write to the commit log fails once, when
/// armed. The volume undoes the partial record and stays usable.
#[derive(Clone)]
struct FlakyLog {
    inner: MemoryStorage,
    armed: Arc<AtomicBool>,
}

struct FlakyFile {
    inner: Box<dyn StorageFile>,
    log: bool,
    armed: Arc<AtomicBool>,
}

impl Storage for FlakyLog {
    fn open(&self, name: &str) -> moul_vfs::Result<Box<dyn StorageFile>> {
        Ok(Box::new(FlakyFile {
            inner: self.inner.open(name)?,
            log: name == "commit.log",
            armed: self.armed.clone(),
        }))
    }

    fn describe(&self) -> String {
        "flaky memory".into()
    }
}

impl StorageFile for FlakyFile {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> moul_vfs::Result<usize> {
        self.inner.read_at(buf, offset)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> moul_vfs::Result<()> {
        if self.log && self.armed.swap(false, Ordering::SeqCst) {
            return Err(moul_vfs::Error::io("injected: the disk is full"));
        }
        self.inner.write_at(buf, offset)
    }

    fn truncate(&mut self, size: u64) -> moul_vfs::Result<()> {
        self.inner.truncate(size)
    }

    fn size(&mut self) -> moul_vfs::Result<u64> {
        self.inner.size()
    }

    fn flush(&mut self) -> moul_vfs::Result<()> {
        self.inner.flush()
    }
}

#[test]
fn a_failed_import_leaves_a_working_store() {
    let storage = FlakyLog {
        inner: MemoryStorage::new(),
        armed: Arc::new(AtomicBool::new(false)),
    };
    let mut s = Store::open(Volume::open(storage.clone()).unwrap()).unwrap();
    s.write(|tx| {
        tx.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('kept');")
    })
    .unwrap();
    let mut other = Store::open_in_memory().unwrap();
    other
        .write(|tx| {
            tx.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('new');")
        })
        .unwrap();
    let file = other.export().unwrap();

    storage.armed.store(true, Ordering::SeqCst);
    let err = s.import(&file).unwrap_err();
    assert!(err.to_string().contains("disk is full"), "{err}");

    // The store still works, with the data it had.
    assert!(s.is_open());
    assert_eq!(note(&s).unwrap(), "kept");
    s.write(|tx| tx.execute("INSERT INTO notes VALUES ('after')", []))
        .unwrap();
    s.import(&file).unwrap();
    assert_eq!(note(&s).unwrap(), "new");
}

#[test]
fn an_import_that_breaks_the_volume_answers_errors_not_panics() {
    let (storage, mut s) = notes_store();
    let file = s.export().unwrap();
    storage.fail_at(Some(storage.steps()));
    assert!(s.import(&file).is_err());

    // The volume cannot be opened again. Later calls answer errors;
    // nothing panics.
    assert!(!s.is_open());
    assert!(note(&s).is_err());
    assert!(matches!(s.format(), Err(Error::Closed(_))));
    let failed: Result<(), rusqlite::Error> = s.write(|tx| {
        tx.execute("INSERT INTO notes VALUES ('lost')", [])
            .map(drop)
    });
    assert!(failed.is_err());
    assert!(s.migrate(|| unreachable!()).is_err());
    assert!(s.migrate_local().is_err());
    assert!(s.ensure_open().is_err());
}

#[test]
fn a_failed_reopen_answers_errors_until_it_succeeds() {
    let (_storage, mut s) = notes_store();
    let good = s.export().unwrap();
    let writer = s.volume().export("writer.db").unwrap();
    // The volume now holds a writer-only store in the shared slot.
    s.reopen_around(|volume| volume.import("main.db", &writer).unwrap())
        .unwrap_err();
    assert!(!s.is_open());
    assert!(note(&s).is_err());
    assert!(s.format().is_err());

    s.reopen_around(|volume| volume.import("main.db", &good).unwrap())
        .unwrap();
    assert!(s.is_open());
    assert_eq!(note(&s).unwrap(), "kept");
}

/// Closes `s` the way a failed replica apply does: the shared slot holds
/// a writer-only store, so opening again fails. Returns a good shared
/// store to put back.
fn close_with_a_bad_reopen(s: &mut Store) -> Vec<u8> {
    let good = s.export().unwrap();
    let writer = s.volume().export("writer.db").unwrap();
    s.reopen_around(|volume| volume.import("main.db", &writer).unwrap())
        .unwrap_err();
    assert!(!s.is_open());
    good
}

#[test]
fn a_write_on_a_closed_store_does_not_open_it_again() {
    let (_storage, mut s) = notes_store();
    let good = close_with_a_bad_reopen(&mut s);
    // The volume could be opened again now, but only the owner may do it.
    s.volume().import("main.db", &good).unwrap();

    let failed: Result<(), rusqlite::Error> = s.write(|tx| {
        tx.execute("INSERT INTO notes VALUES ('quiet')", [])
            .map(drop)
    });
    let err = failed.unwrap_err();
    assert!(err.to_string().contains("closed"), "{err}");
    assert!(!s.is_open());
    assert!(matches!(
        s.migrate(|| unreachable!()),
        Err(Error::Closed(_))
    ));
    assert!(matches!(s.migrate_local(), Err(Error::Closed(_))));
    assert!(!s.is_open());

    s.ensure_open().unwrap();
    assert!(s.is_open());
    assert_eq!(note(&s).unwrap(), "kept");
}

/// A hook that counts its runs and installs an authorizer refusing any
/// read of the writer-only store, like an owner's SQL fence.
fn fence(runs: Arc<AtomicUsize>) -> impl FnMut(&mut Connection) -> rusqlite::Result<()> + Send {
    move |conn: &mut Connection| {
        runs.fetch_add(1, Ordering::SeqCst);
        conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.database_name {
            Some("writer") => Authorization::Deny,
            _ => Authorization::Allow,
        }))
    }
}

fn reads_writer(s: &Store) -> bool {
    s.conn()
        .query_row("SELECT count(*) FROM writer.sqlite_schema", [], |r| {
            r.get::<_, i64>(0)
        })
        .is_ok()
}

#[test]
fn the_connect_hook_runs_on_every_new_connection() {
    let (_storage, mut s) = notes_store();
    let runs = Arc::new(AtomicUsize::new(0));
    assert!(reads_writer(&s));
    s.on_connect(fence(runs.clone())).unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 1, "runs at once");
    assert!(!reads_writer(&s));

    let file = s.export().unwrap();
    s.import(&file).unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 2, "after an import");
    assert!(!reads_writer(&s));

    s.reopen_around(|_| ()).unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 3, "after reopen_around");
    assert!(!reads_writer(&s));

    let good = close_with_a_bad_reopen(&mut s);
    assert_eq!(runs.load(Ordering::SeqCst), 3, "not on a failed open");
    s.volume().import("main.db", &good).unwrap();
    s.ensure_open().unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 4, "after ensure_open");
    assert!(!reads_writer(&s));
    assert_eq!(note(&s).unwrap(), "kept");
}

#[test]
fn a_failing_connect_hook_keeps_the_store_closed() {
    let (_storage, mut s) = notes_store();
    let fail = Arc::new(AtomicBool::new(false));
    let flag = fail.clone();
    s.on_connect(move |_| {
        if flag.load(Ordering::SeqCst) {
            Err(rusqlite::Error::InvalidQuery)
        } else {
            Ok(())
        }
    })
    .unwrap();

    fail.store(true, Ordering::SeqCst);
    assert!(s.reopen_around(|_| ()).is_err());
    assert!(!s.is_open());
    assert!(note(&s).is_err());
    assert!(s.ensure_open().is_err());

    fail.store(false, Ordering::SeqCst);
    s.ensure_open().unwrap();
    assert_eq!(note(&s).unwrap(), "kept");

    // A hook that fails at once is not kept.
    assert!(
        s.on_connect(|_| Err(rusqlite::Error::InvalidQuery))
            .is_err()
    );
    s.reopen_around(|_| ()).unwrap();
}

#[test]
fn the_stand_in_refuses_writes_even_with_its_authorizer_replaced() {
    let (_storage, mut s) = notes_store();
    close_with_a_bad_reopen(&mut s);
    s.conn_mut()
        .authorizer(Some(|_: AuthContext<'_>| Authorization::Allow))
        .unwrap();
    let err = s
        .conn()
        .execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('lost')")
        .unwrap_err();
    assert!(err.to_string().contains("readonly"), "{err}");
}

#[test]
fn a_store_in_the_wrong_slot_is_refused() {
    let storage = MemoryStorage::new();
    drop(store(&storage));
    let device = storage.file("device.db").unwrap();
    storage.set_file("main.db", &device);
    let err = Store::open(Volume::open(storage).unwrap()).unwrap_err();
    assert!(
        matches!(
            err,
            Error::WrongStore {
                expected: StoreKind::Shared,
                found: Some(StoreKind::Device)
            }
        ),
        "{err}"
    );
}

#[test]
fn close_hands_back_the_volume() {
    let storage = MemoryStorage::new();
    let s = store(&storage);
    let volume = s.close().unwrap();
    let s = Store::open(volume).unwrap();
    assert_eq!(s.volume().last_commit().0, 1);
}
