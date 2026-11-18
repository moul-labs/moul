//! What sync needs from the device's own database.
//!
//! [`Local`] is the boundary between sync, which does network I/O, and the
//! local database, which is synchronous. Each call does one short piece of
//! work, so a host that shares the database between requests and sync (the
//! browser worker) can lend it out call by call. [`StoreLocal`] implements it
//! for a [`Store`] and is what the CLI and the tests use; the `store_*`
//! functions are its parts, for hosts that implement the trait themselves.

use std::path::PathBuf;

use moul_ltx::Txid;
use moul_store::Store;
use moul_vfs::{ApplyMode, Capture, Position};
use serde::{Deserialize, Serialize};

use crate::replica::BoxFuture;
use crate::{Error, ErrorKind, Result};

/// A device's part in a replica.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Not connected to a replica.
    #[default]
    None,
    /// The device writes and pushes.
    Writer,
    /// The device follows another writer and cannot write.
    Follower,
}

/// What a device remembers about its replica, kept in its device store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceState {
    /// This device's ID, chosen once.
    pub device: String,
    /// The ID of the replica the shared store came from, if any.
    #[serde(default)]
    pub replica: Option<String>,
    /// The device's role when it last knew.
    #[serde(default)]
    pub role: Role,
    /// The writer generation when it last knew.
    #[serde(default)]
    pub generation: u64,
}

/// A copy of the local database kept aside because it held changes the
/// replica does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictCopy {
    /// A file name for it, such as
    /// `conflict-2026-11-18T03-04-05Z-txid-12-15.db`.
    pub name: String,
    /// The database file.
    pub bytes: Vec<u8>,
    /// The first TXID the replica does not hold.
    pub from: Txid,
    /// The last local TXID.
    pub to: Txid,
    /// Why it was kept.
    pub reason: String,
}

/// The device's database, as sync uses it.
pub trait Local {
    /// The capture of the shared store.
    fn capture(&self) -> Capture;
    /// Applies a replica file; see [`Capture::apply`].
    fn apply(&mut self, ltx: &[u8], mode: ApplyMode) -> Result<Position>;
    /// Writes a fence file; see [`Capture::fence`].
    fn fence(&mut self) -> Result<Txid>;
    /// Starts a new replica's numbering; see [`Capture::rebase`].
    fn rebase(&mut self) -> Result<Option<Txid>>;
    /// The shared store as a SQLite file.
    fn export(&mut self) -> Result<Vec<u8>>;
    /// Whether the shared store holds any table, index or view.
    fn has_data(&mut self) -> Result<bool>;
    /// Reads the device state.
    fn load_state(&mut self) -> Result<Option<DeviceState>>;
    /// Saves the device state.
    fn save_state(&mut self, state: &DeviceState) -> Result<()>;
    /// Keeps `copy` somewhere the builder can find it, durably, and returns
    /// where. Sync replaces the database only after this returns.
    fn save_conflict(&mut self, copy: ConflictCopy) -> BoxFuture<'_, Result<String>>;
    /// The device became `role`. A host makes the database read-only for a
    /// follower here.
    fn role_changed(&mut self, _role: Role) {}
}

const STATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS device.moul_sync (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
)";

/// Reads a value from the device store's sync table.
pub fn store_get(store: &Store, key: &str) -> Result<Option<String>> {
    store.conn().execute(STATE_TABLE, [])?;
    let mut stmt = store
        .conn()
        .prepare_cached("SELECT value FROM device.moul_sync WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    Ok(match rows.next()? {
        Some(row) => Some(row.get(0)?),
        None => None,
    })
}

/// Writes a value to the device store's sync table, or removes it.
pub fn store_set(store: &Store, key: &str, value: Option<&str>) -> Result<()> {
    store.conn().execute(STATE_TABLE, [])?;
    match value {
        Some(v) => store.conn().execute(
            "INSERT INTO device.moul_sync (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, v],
        )?,
        None => store
            .conn()
            .execute("DELETE FROM device.moul_sync WHERE key = ?1", [key])?,
    };
    Ok(())
}

/// [`Local::load_state`] for a store.
pub fn store_load_state(store: &Store) -> Result<Option<DeviceState>> {
    match store_get(store, "state")? {
        None => Ok(None),
        Some(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| Error::new(ErrorKind::Local, format!("bad sync state: {e}"))),
    }
}

/// [`Local::save_state`] for a store.
pub fn store_save_state(store: &Store, state: &DeviceState) -> Result<()> {
    let json = serde_json::to_string(state).expect("state serializes");
    store_set(store, "state", Some(&json))
}

/// [`Local::apply`] for a store: replacing the database closes and reopens
/// the connection around it.
pub fn store_apply(
    store: &mut Store,
    capture: &Capture,
    ltx: &[u8],
    mode: ApplyMode,
) -> Result<Position> {
    let snapshot = moul_ltx::Ltx::decode(ltx)?.header().is_snapshot();
    if mode == ApplyMode::Replace || snapshot {
        Ok(store.reopen_around(|volume| capture.apply(volume, ltx, mode))??)
    } else {
        Ok(capture.apply(store.volume(), ltx, mode)?)
    }
}

/// [`Local::has_data`] for a store.
pub fn store_has_data(store: &Store) -> Result<bool> {
    let n: i64 = store
        .conn()
        .query_row("SELECT count(*) FROM main.sqlite_schema", [], |r| r.get(0))?;
    Ok(n > 0)
}

/// A store and its capture, with conflict copies kept in a folder.
pub struct StoreLocal<'a> {
    /// The stores.
    pub store: &'a mut Store,
    /// The capture of `main.db`.
    pub capture: Capture,
    /// Where conflict copies go. `None` refuses to keep one, so sync stops
    /// rather than replace changes it cannot keep. Only used natively.
    pub conflicts: Option<PathBuf>,
}

impl std::fmt::Debug for StoreLocal<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreLocal")
            .field("capture", &self.capture)
            .finish_non_exhaustive()
    }
}

impl Local for StoreLocal<'_> {
    fn capture(&self) -> Capture {
        self.capture.clone()
    }

    fn apply(&mut self, ltx: &[u8], mode: ApplyMode) -> Result<Position> {
        store_apply(self.store, &self.capture, ltx, mode)
    }

    fn fence(&mut self) -> Result<Txid> {
        Ok(self.capture.fence(self.store.volume())?)
    }

    fn rebase(&mut self) -> Result<Option<Txid>> {
        Ok(self.capture.rebase(self.store.volume())?)
    }

    fn export(&mut self) -> Result<Vec<u8>> {
        Ok(self.store.export()?)
    }

    fn has_data(&mut self) -> Result<bool> {
        store_has_data(self.store)
    }

    fn load_state(&mut self) -> Result<Option<DeviceState>> {
        store_load_state(self.store)
    }

    fn save_state(&mut self, state: &DeviceState) -> Result<()> {
        store_save_state(self.store, state)
    }

    fn save_conflict(&mut self, copy: ConflictCopy) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(dir) = &self.conflicts {
                let fail = |e: std::io::Error| {
                    Error::new(ErrorKind::Local, format!("cannot keep {}: {e}", copy.name))
                };
                std::fs::create_dir_all(dir).map_err(fail)?;
                let path = dir.join(&copy.name);
                let tmp = dir.join(format!(".{}.tmp", copy.name));
                {
                    use std::io::Write;
                    let mut f = std::fs::File::create(&tmp).map_err(fail)?;
                    f.write_all(&copy.bytes)
                        .and_then(|()| f.sync_all())
                        .map_err(fail)?;
                }
                std::fs::rename(&tmp, &path).map_err(fail)?;
                return Ok(path.display().to_string());
            }
            Err(Error::new(
                ErrorKind::Local,
                format!(
                    "this device holds TXIDs {}-{} the replica does not ({}), and has nowhere to keep them",
                    copy.from, copy.to, copy.reason
                ),
            ))
        })
    }
}
