//! The three-store layout, export and import.

#![cfg(not(target_arch = "wasm32"))]

use moul_store::{Error, FILES, Store, StoreKind};
use moul_vfs::{MemoryStorage, Volume};

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
