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
//! [`schema`] is the app definition (collections, fields, rules),
//! [`catalog`] stores it and keeps each collection's table in line with it,
//! and [`system`] holds the system tables of each store, their migrations,
//! and the conversion of a Go engine database, which keeps everything in one
//! file, into three stores.

use std::fmt;

pub mod catalog;
pub mod schema;
pub mod system;

pub use system::{FORMAT, GoConversion, Migrated};

use moul_vfs::{CommitSeq, MemoryStorage, PAGE_SIZE, Volume};
use rusqlite::hooks::{AuthContext, Authorization};
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
    /// Stored data cannot be read. The message says what.
    Corrupt(String),
    /// A change conflicts with the data, such as a unique index over
    /// duplicate values. The message is meant for the builder.
    Conflict(String),
    /// The database was written by a newer engine.
    TooNew(String),
    /// The stores could not be opened again after an import or a replica
    /// apply. The message says why. [`Store::ensure_open`] tries again.
    Closed(String),
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
            Error::Corrupt(why) | Error::Conflict(why) | Error::TooNew(why) => f.write_str(why),
            Error::Closed(why) => write!(f, "the store is closed: {why}"),
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

/// Per-connection settings the owner puts on each new connection. See
/// [`Store::on_connect`].
type ConnectHook = Box<dyn FnMut(&mut Connection) -> rusqlite::Result<()> + Send>;

/// A Moul database: three stores on one volume, behind one connection.
pub struct Store {
    // Declared before `volume` so the connection closes first. While
    // `closed` is set, this is a stand-in that refuses every statement.
    conn: Connection,
    closed: Option<Closed>,
    on_connect: Option<ConnectHook>,
    volume: Volume,
}

/// Why the stores are closed, and how to open them again.
#[derive(Debug)]
struct Closed {
    adopt: bool,
    why: String,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("volume", &self.volume)
            .field("closed", &self.closed)
            .field("on_connect", &self.on_connect.is_some())
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens the stores in `volume`, creating and marking any that are new.
    pub fn open(volume: Volume) -> Result<Store> {
        let conn = connect(&volume, true)?;
        Ok(Store {
            conn,
            closed: None,
            on_connect: None,
            volume,
        })
    }

    /// Opens the stores in `volume` like [`open`](Self::open), but never
    /// writes to the shared store: a plain SQLite file there is left
    /// unmarked. A device that follows a replica opens its stores this way,
    /// because its shared store must stay byte for byte the writer's.
    pub fn open_follower(volume: Volume) -> Result<Store> {
        let conn = connect(&volume, false)?;
        Ok(Store {
            conn,
            closed: None,
            on_connect: None,
            volume,
        })
    }

    /// Runs `hook` on the connection now, and on every new connection when
    /// the stores are opened again: by [`import`](Self::import),
    /// [`reopen_around`](Self::reopen_around) or
    /// [`ensure_open`](Self::ensure_open). A new connection starts without
    /// any per-connection setting, such as an authorizer, so the owner puts
    /// those here. It replaces a hook set before.
    ///
    /// If `hook` fails now, it is not kept. If it fails on a new connection,
    /// that connection is closed and the stores stay closed, so no
    /// connection is ever handed out without it. While the stores are
    /// closed, `hook` only runs once they open.
    pub fn on_connect(
        &mut self,
        mut hook: impl FnMut(&mut Connection) -> rusqlite::Result<()> + Send + 'static,
    ) -> Result<()> {
        if self.closed.is_none() {
            hook(&mut self.conn)?;
        }
        self.on_connect = Some(Box::new(hook));
        Ok(())
    }

    /// Closes the connection, runs `f` on the volume, then opens the
    /// connection again without writing to the shared store, so SQLite
    /// rereads everything. For changes made underneath SQLite that it might
    /// not notice, such as replacing `main.db` with a replica's snapshot.
    ///
    /// If opening again fails, the store stays closed: see
    /// [`is_open`](Self::is_open).
    pub fn reopen_around<T>(&mut self, f: impl FnOnce(&Volume) -> T) -> Result<T> {
        self.disconnect(false)?;
        let value = f(&self.volume);
        self.reconnect(false)?;
        Ok(value)
    }

    /// Whether the stores are open. They are closed only when opening them
    /// again failed after an import or a replica apply, for example on a
    /// volume that a failed commit left unusable. A closed store answers
    /// errors instead: [`write`](Self::write), [`migrate`](Self::migrate),
    /// [`migrate_local`](Self::migrate_local) and [`format`](Self::format)
    /// return [`Error::Closed`], and [`conn`](Self::conn) refuses every
    /// statement. None of them opens the stores again behind the owner's
    /// back; only [`ensure_open`](Self::ensure_open) does.
    pub fn is_open(&self) -> bool {
        self.closed.is_none()
    }

    /// Opens the stores again if they are closed, and runs the
    /// [`on_connect`](Self::on_connect) hook on the new connection. Does
    /// nothing if they are open.
    pub fn ensure_open(&mut self) -> Result<()> {
        match &self.closed {
            None => Ok(()),
            Some(closed) => {
                let adopt = closed.adopt;
                self.reconnect(adopt)
            }
        }
    }

    /// [`Error::Closed`] if the stores are closed.
    fn check_open(&self) -> Result<()> {
        match &self.closed {
            None => Ok(()),
            Some(closed) => Err(Error::Closed(closed.why.clone())),
        }
    }

    /// Puts a stand-in in place of the connection and closes the
    /// connection. If closing fails, the connection stays.
    fn disconnect(&mut self, adopt: bool) -> Result<()> {
        let conn = std::mem::replace(&mut self.conn, stand_in()?);
        if let Err((conn, e)) = conn.close() {
            self.conn = conn;
            return Err(e.into());
        }
        // Until reconnect succeeds, for example if `f` of reopen_around
        // panics.
        self.closed = Some(Closed {
            adopt,
            why: "the stores are being reopened".into(),
        });
        Ok(())
    }

    /// Opens the stores in place of the stand-in and runs the
    /// [`on_connect`](Self::on_connect) hook. On failure they stay closed,
    /// and the error is kept for [`Error::Closed`].
    fn reconnect(&mut self, adopt: bool) -> Result<()> {
        let hook = &mut self.on_connect;
        let connected = connect(&self.volume, adopt).and_then(|mut conn| {
            if let Some(hook) = hook {
                // On failure the new connection is dropped, which closes it.
                hook(&mut conn)?;
            }
            Ok(conn)
        });
        match connected {
            Ok(conn) => {
                self.conn = conn;
                self.closed = None;
                Ok(())
            }
            Err(e) => {
                self.closed = Some(Closed {
                    adopt,
                    why: e.to_string(),
                });
                Err(e)
            }
        }
    }

    /// Opens a store that lives only in memory, for a private window or a
    /// test. Nothing survives the store being dropped.
    pub fn open_in_memory() -> Result<Store> {
        Store::open(Volume::open(MemoryStorage::new())?)
    }

    /// The connection, with `writer` and `device` attached. While the
    /// stores are closed (see [`is_open`](Self::is_open)), a stand-in that
    /// refuses every statement.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The connection, mutably, for settings that need it such as SQLite's
    /// progress handler. A stand-in while the stores are closed, as for
    /// [`conn`](Self::conn): it is read-only, so it still refuses writes
    /// if its authorizer is replaced. A setting that must hold on every
    /// connection belongs in [`on_connect`](Self::on_connect), because the
    /// connection is replaced when the stores are opened again.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// The volume underneath.
    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Runs `f` in one write transaction across all three stores. It commits
    /// if `f` returns `Ok` and rolls back otherwise. If the stores are
    /// closed, it fails with [`Error::Closed`], given as an SQLite error
    /// (`SQLITE_CANTOPEN`); the owner opens them again with
    /// [`ensure_open`](Self::ensure_open).
    pub fn write<T, E>(
        &mut self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<rusqlite::Error>,
    {
        self.check_open().map_err(|e| E::from(as_sqlite_error(e)))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
    ///
    /// The stores are opened again whether or not the import landed. If
    /// that fails too, they stay closed: see [`is_open`](Self::is_open).
    pub fn import(&mut self, bytes: &[u8]) -> Result<CommitSeq> {
        check_import(bytes)?;
        // Close the connection so that SQLite rereads everything afterwards.
        self.disconnect(true)?;
        let imported = self.volume.import(StoreKind::Shared.file_name(), bytes);
        let reopened = self.reconnect(true);
        let seq = imported?;
        reopened?;
        Ok(seq)
    }

    /// Closes the connection and returns the volume.
    pub fn close(self) -> Result<Volume> {
        self.conn.close().map_err(|(_, e)| e)?;
        Ok(self.volume)
    }
}

/// A connection to an empty database in memory that refuses every
/// statement. It stands in for the stores while they are closed. It is
/// read-only as well, so it still refuses writes if a caller replaces its
/// authorizer through [`Store::conn_mut`].
fn stand_in() -> Result<Connection> {
    let mut conn = Connection::open_in_memory_with_flags(
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.authorizer(Some(|_: AuthContext<'_>| Authorization::Deny))?;
    Ok(conn)
}

/// `e` as an SQLite error, for [`Store::write`], whose callers convert
/// from those.
fn as_sqlite_error(e: Error) -> rusqlite::Error {
    match e {
        Error::Sqlite(e) => e,
        other => rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(other.to_string()),
        ),
    }
}

/// Opens `main.db` on `volume`, attaches the other stores and checks them.
/// With `adopt`, an unmarked shared store is marked as one; without it, the
/// shared store is never written.
fn connect(volume: &Volume, adopt: bool) -> Result<Connection> {
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
                if adopt || kind != StoreKind::Shared {
                    fresh.push(kind);
                }
            }
            0 if kind == StoreKind::Shared && !adopt => {}
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
