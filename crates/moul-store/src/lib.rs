//! SQLite access for Moul: the three stores of a database, opened as one
//! connection.
//!
//! | Store        | File        | Schema   | Holds                                         |
//! | ------------ | ----------- | -------- | --------------------------------------------- |
//! | Shared       | `main.db`   | `main`   | Records, the app definition, the change log   |
//! | Writer-only  | `writer.db` | `writer` | Secrets and the request log                   |
//! | Device       | `device.db` | `device` | Unconfirmed requests, drafts, sync position   |
//!
//! `main.db` is the file the builder exports, and the one that is replicated
//! to every device. The others are attached to the same connection, so a
//! transaction can write a record and a password hash together, and the
//! [`Volume`] commits both or neither.
//!
//! Each file is marked with its own SQLite `application_id`, so a store can
//! never be opened in another's place. Page size is 4,096 bytes everywhere.
//!
//! System tables and migrations arrive with the engine port in Phase 4,
//! including converting a Go engine database, which keeps everything in one
//! file, into three stores.

use std::fmt;

use moul_vfs::{CommitSeq, MemoryStorage, PAGE_SIZE, Volume};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

/// One of the three files of a Moul database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StoreKind {
    /// `main.db`: the shared data every device and the replica get.
    Shared,
    /// `writer.db`: secrets and logs only the writer holds.
    WriterOnly,
    /// `device.db`: what belongs to this device alone.
    Device,
}

impl StoreKind {
    /// The three stores, in the order they are attached.
    pub const ALL: [StoreKind; 3] = [StoreKind::Shared, StoreKind::WriterOnly, StoreKind::Device];

    /// The file name inside the volume.
    pub const fn file_name(self) -> &'static str {
        match self {
            StoreKind::Shared => "main.db",
            StoreKind::WriterOnly => "writer.db",
            StoreKind::Device => "device.db",
        }
    }

    /// The schema name on the connection: `main`, `writer` or `device`.
    pub const fn schema(self) -> &'static str {
        match self {
            StoreKind::Shared => "main",
            StoreKind::WriterOnly => "writer",
            StoreKind::Device => "device",
        }
    }

    /// The SQLite `application_id` that marks the file: "MOUL", "MOUW", "MOUD".
    pub const fn application_id(self) -> i32 {
        i32::from_be_bytes(match self {
            StoreKind::Shared => *b"MOUL",
            StoreKind::WriterOnly => *b"MOUW",
            StoreKind::Device => *b"MOUD",
        })
    }

    fn from_application_id(id: i32) -> Option<StoreKind> {
        StoreKind::ALL
            .into_iter()
            .find(|k| k.application_id() == id)
    }
}

impl fmt::Display for StoreKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StoreKind::Shared => "shared store",
            StoreKind::WriterOnly => "writer-only store",
            StoreKind::Device => "device store",
        })
    }
}

/// The names of the files a store keeps in its volume. A browser back end
/// opens these in advance.
pub const FILES: [&str; 3] = [
    StoreKind::Shared.file_name(),
    StoreKind::WriterOnly.file_name(),
    StoreKind::Device.file_name(),
];

/// What can go wrong opening or using a store.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// SQLite reported an error.
    Sqlite(rusqlite::Error),
    /// The file layer reported an error.
    Vfs(moul_vfs::Error),
    /// A file is a different store, or not a Moul store at all.
    WrongStore {
        /// The slot the file was opened in.
        expected: StoreKind,
        /// What its `application_id` says it is, if it is a Moul store.
        found: Option<StoreKind>,
    },
    /// A file to import is not acceptable. The message says why.
    BadImport(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sqlite(e) => write!(f, "SQLite: {e}"),
            Error::Vfs(e) => write!(f, "{e}"),
            Error::WrongStore {
                expected,
                found: Some(found),
            } => write!(
                f,
                "{} holds a {found}, not the {expected}",
                expected.file_name()
            ),
            Error::WrongStore {
                expected,
                found: None,
            } => write!(f, "{} is not a Moul {expected}", expected.file_name()),
            Error::BadImport(why) => write!(f, "cannot import: {why}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Sqlite(e) => Some(e),
            Error::Vfs(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e)
    }
}

impl From<moul_vfs::Error> for Error {
    fn from(e: moul_vfs::Error) -> Self {
        Error::Vfs(e)
    }
}

/// Result type of this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A Moul database: three stores on one volume, behind one connection.
pub struct Store {
    // Declared before `volume` so the connection closes first.
    conn: Option<Connection>,
    volume: Volume,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("volume", &self.volume)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens the stores in `volume`, creating and marking any that are new.
    pub fn open(volume: Volume) -> Result<Store> {
        let conn = connect(&volume)?;
        Ok(Store {
            conn: Some(conn),
            volume,
        })
    }

    /// Opens a store that lives only in memory, for a private window or a
    /// test. Nothing survives the store being dropped.
    pub fn open_in_memory() -> Result<Store> {
        Store::open(Volume::open(MemoryStorage::new())?)
    }

    /// The connection, with `writer` and `device` attached.
    pub fn conn(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("the connection is only absent during import")
    }

    /// The volume underneath.
    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Runs `f` in one write transaction across all three stores. It commits
    /// if `f` returns `Ok` and rolls back otherwise.
    pub fn write<T, E>(
        &mut self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<rusqlite::Error>,
    {
        let conn = self.conn.as_mut().expect("connection present");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// The shared store as a SQLite file, as of the last commit.
    pub fn export(&self) -> Result<Vec<u8>> {
        Ok(self.volume.export(StoreKind::Shared.file_name())?)
    }

    /// Replaces the shared store with `bytes`, a file from [`export`](Self::export)
    /// or any SQLite database with 4,096-byte pages. The writer-only and
    /// device stores are kept.
    ///
    /// The file is checked in a scratch volume first: it must pass SQLite's
    /// integrity check and must not be another kind of Moul store. Then it
    /// lands as one commit, so a crash leaves either the old or the new data.
    pub fn import(&mut self, bytes: &[u8]) -> Result<CommitSeq> {
        check_import(bytes)?;
        // Close the connection so that SQLite rereads everything afterwards.
        self.conn = None;
        let imported = self.volume.import(StoreKind::Shared.file_name(), bytes);
        let reopened = connect(&self.volume);
        let seq = imported?;
        self.conn = Some(reopened?);
        Ok(seq)
    }

    /// Closes the connection and returns the volume.
    pub fn close(mut self) -> Result<Volume> {
        if let Some(conn) = self.conn.take() {
            conn.close().map_err(|(_, e)| e)?;
        }
        Ok(self.volume)
    }
}

/// Opens `main.db` on `volume`, attaches the other stores and checks them.
fn connect(volume: &Volume) -> Result<Connection> {
    let conn = Connection::open_with_flags_and_vfs(
        StoreKind::Shared.file_name(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        volume.vfs_name(),
    )?;
    for kind in &StoreKind::ALL[1..] {
        conn.execute(
            &format!("ATTACH ?1 AS {}", kind.schema()),
            [kind.file_name()],
        )?;
    }
    // The volume's commit log makes commits atomic and durable, so SQLite
    // keeps its rollback journal in memory and skips its own syncs. Locking
    // stays NORMAL: the volume sees a commit end when the write lock drops.
    let mode: String = conn.query_row("PRAGMA journal_mode = MEMORY", [], |r| r.get(0))?;
    if mode != "memory" {
        return Err(Error::Vfs(moul_vfs::Error::invalid(format!(
            "SQLite kept journal mode {mode:?}"
        ))));
    }
    conn.execute_batch(
        "PRAGMA synchronous = OFF;
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;",
    )?;

    let mut fresh = Vec::new();
    for kind in StoreKind::ALL {
        let schema = kind.schema();
        let id: i32 =
            conn.query_row(&format!("PRAGMA {schema}.application_id"), [], |r| r.get(0))?;
        let objects: i64 = conn.query_row(
            &format!("SELECT count(*) FROM {schema}.sqlite_schema"),
            [],
            |r| r.get(0),
        )?;
        match id {
            0 if objects == 0 => {
                conn.execute_batch(&format!("PRAGMA {schema}.page_size = {PAGE_SIZE}"))?;
                fresh.push(kind);
            }
            // A plain SQLite file in the shared slot, as imported: adopt it.
            0 if kind == StoreKind::Shared => fresh.push(kind),
            id if id == kind.application_id() => {}
            id => {
                return Err(Error::WrongStore {
                    expected: kind,
                    found: StoreKind::from_application_id(id),
                });
            }
        }
    }
    if !fresh.is_empty() {
        let mut sql = String::from("BEGIN IMMEDIATE;");
        for kind in &fresh {
            sql += &format!(
                "PRAGMA {}.application_id = {};",
                kind.schema(),
                kind.application_id()
            );
        }
        sql += "COMMIT;";
        conn.execute_batch(&sql)?;
    }
    for kind in StoreKind::ALL {
        let ps: u32 = conn.query_row(&format!("PRAGMA {}.page_size", kind.schema()), [], |r| {
            r.get(0)
        })?;
        if ps != PAGE_SIZE {
            return Err(Error::Vfs(moul_vfs::Error::invalid(format!(
                "{} has {ps}-byte pages; Moul stores use {PAGE_SIZE}",
                kind.file_name()
            ))));
        }
    }
    Ok(conn)
}

/// Checks a file before it replaces the shared store.
fn check_import(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 100 || !bytes.starts_with(b"SQLite format 3\0") {
        return Err(Error::BadImport("not a SQLite database file".into()));
    }
    let raw = u16::from_be_bytes([bytes[16], bytes[17]]) as u32;
    let ps = if raw == 1 { 65536 } else { raw };
    if ps != PAGE_SIZE {
        return Err(Error::BadImport(format!(
            "the file has {ps}-byte pages; Moul stores use {PAGE_SIZE}. \
             Convert it with `PRAGMA page_size = {PAGE_SIZE}; VACUUM;` first"
        )));
    }

    let scratch = Volume::open(MemoryStorage::new())?;
    scratch
        .import("check.db", bytes)
        .map_err(|e| Error::BadImport(e.message().to_owned()))?;
    let conn = Connection::open_with_flags_and_vfs(
        "check.db",
        OpenFlags::SQLITE_OPEN_READ_WRITE,
        scratch.vfs_name(),
    )?;
    let checked: rusqlite::Result<(String, i32)> = (|| {
        let integrity: String = conn.query_row("PRAGMA integrity_check(1)", [], |r| r.get(0))?;
        let id: i32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        Ok((integrity, id))
    })();
    drop(conn);
    let (integrity, id) =
        checked.map_err(|e| Error::BadImport(format!("SQLite cannot read it: {e}")))?;
    if integrity != "ok" {
        return Err(Error::BadImport(format!(
            "integrity check failed: {integrity}"
        )));
    }
    match StoreKind::from_application_id(id) {
        None if id == 0 => Ok(()),
        Some(StoreKind::Shared) => Ok(()),
        Some(other) => Err(Error::BadImport(format!(
            "this is a {other} ({}), not a shared store",
            other.file_name()
        ))),
        None => Err(Error::BadImport(format!(
            "the file belongs to another application (application_id {id:#x})"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_ids_spell_their_store() {
        assert_eq!(StoreKind::Shared.application_id().to_be_bytes(), *b"MOUL");
        assert_eq!(
            StoreKind::from_application_id(StoreKind::Device.application_id()),
            Some(StoreKind::Device)
        );
    }
}
