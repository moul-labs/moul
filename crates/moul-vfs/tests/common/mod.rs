//! Helpers shared by the integration tests.

#![allow(dead_code)]

use moul_vfs::Volume;
use rusqlite::{Connection, OpenFlags};

/// Opens `main.db` on `volume` with the pragmas Moul requires, and attaches
/// `writer.db` as `writer`.
pub fn connect(volume: &Volume) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags_and_vfs(
        "main.db",
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        volume.vfs_name(),
    )?;
    conn.execute_batch(
        "PRAGMA page_size = 4096;
         PRAGMA synchronous = OFF;
         PRAGMA temp_store = MEMORY;
         ATTACH 'writer.db' AS writer;
         PRAGMA writer.page_size = 4096;",
    )?;
    let mode: String = conn.query_row("PRAGMA journal_mode = MEMORY", [], |r| r.get(0))?;
    assert_eq!(mode, "memory");
    Ok(conn)
}

pub fn integrity(conn: &Connection) -> String {
    conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap()
}

pub fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
