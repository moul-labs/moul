//! Opening a database folder for the server.
//!
//! ```text
//! <folder>/main.db     the shared store, a SQLite file other tools can read
//! <folder>/writer.db   the writer-only store: secrets, the request log
//! <folder>/device.db   this server's own state
//! <folder>/ltx/        level-0 LTX files not yet pushed to the replica
//! <folder>/files/      uploaded files, named by SHA-256
//! <folder>/moul.lock   held while a process uses the folder
//! ```

use std::fs::{self, File};
use std::path::Path;

use moul_core::{Config, DiskBlobs, Engine, SystemHost};
use moul_store::Store;
use moul_vfs::{Capture, DiskStorage, Volume};
use rusqlite::OptionalExtension;

use crate::config::ServerConfig;

/// An open database folder.
pub struct Opened {
    /// The engine.
    pub engine: Engine,
    /// The capture of `main.db`, which replication pushes from.
    pub capture: Capture,
    /// The admin key in force.
    pub admin_key: String,
    /// Held while the folder is in use.
    pub lock: File,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

/// Takes the folder's lock, so that one process uses it at a time.
pub fn lock(dir: &Path) -> Result<File, String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("moul.lock");
    let file = File::create(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.try_lock()
        .map_err(|_| format!("another moul process is using {}", dir.display()))?;
    Ok(file)
}

/// Makes an admin key: 32 letters and digits.
fn new_admin_key() -> String {
    moul_core::ids::random_chars(&mut OsRandom, 32)
}

/// Randomness from the operating system, for ids made outside the engine.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRandom;

impl moul_core::Host for OsRandom {
    fn now_ms(&mut self) -> i64 {
        moul_vfs::system_clock()
    }
    fn fill_random(&mut self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("the operating system supplies randomness");
    }
}

/// Opens `config.dir`, creating it if needed. Returns the engine, the
/// capture and the admin key, and whether that key was just made.
pub fn open(config: &ServerConfig) -> Result<(Opened, bool), String> {
    let dir = &config.dir;
    let lock = lock(dir)?;
    let storage = DiskStorage::new(dir).map_err(|e| e.to_string())?;
    let ltx = DiskStorage::new(dir.join("ltx")).map_err(|e| e.to_string())?;
    let capture = Capture::open(ltx, "main.db").map_err(|e| e.to_string())?;
    let volume = Volume::open_observed(storage, capture.clone()).map_err(|e| e.to_string())?;
    let mut store = if config.follower {
        Store::open_follower(volume)
    } else {
        Store::open(volume)
    }
    .map_err(|e| e.to_string())?;
    // The writer-only store first: the admin key lives there.
    store.migrate_local().map_err(|e| e.to_string())?;
    // A folder `moul pull` keeps follows its replica's writer; writing to
    // it here would fork the database.
    if !config.follower
        && config.replica.is_none()
        && moul_sync::store_load_state(&store)
            .ok()
            .flatten()
            .is_some_and(|s| s.role == moul_sync::Role::Follower)
    {
        return Err(format!(
            "{} follows a replica's writer (moul pull); serve it with --read-only, or with --replica to take part in its sync",
            dir.display()
        ));
    }
    let (admin_key, made) = match &config.admin_key {
        Some(k) => (k.clone(), false),
        None => {
            let stored: Option<Vec<u8>> = store
                .conn()
                .query_row(
                    "SELECT value FROM writer._keys WHERE name = 'admin'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            match stored.and_then(|v| String::from_utf8(v).ok()) {
                Some(k) => (k, false),
                None => {
                    let k = new_admin_key();
                    store
                        .conn()
                        .execute(
                            "INSERT INTO writer._keys (name, value) VALUES ('admin', ?1)",
                            [k.as_bytes()],
                        )
                        .map_err(|e| e.to_string())?;
                    (k, true)
                }
            }
        }
    };
    let engine_config = Config {
        admin_key: Some(admin_key.clone()),
        token_ttl_ms: config.token_ttl_ms,
        refresh_grace_ms: config.refresh_grace_ms,
        public_url: config.public_url(),
        file_url_ttl_ms: config.file_url_ttl_ms,
        signing_key: config.signing_key.clone(),
        ..Config::default()
    };
    let blobs = DiskBlobs::new(dir.join("files")).map_err(|e| e.to_string())?;
    let engine = if config.follower {
        Engine::open_follower(store, SystemHost, blobs, engine_config)
    } else {
        Engine::open(store, SystemHost, blobs, engine_config)
    }
    .map_err(|e| e.to_string())?;
    let mut engine = engine;
    if let Some(path) = &config.import {
        let used: i64 = engine
            .store()
            .conn()
            .query_row(
                "SELECT (SELECT count(*) FROM main._moul) + (SELECT count(*) FROM main._changes)",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if used > 0 {
            return Err(format!(
                "{} already holds a database; --from fills only a new folder",
                dir.display()
            ));
        }
        let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        engine
            .import_database(&bytes)
            .map_err(|e| format!("cannot import {}: {e}", path.display()))?;
    }
    Ok((
        Opened {
            engine,
            capture,
            admin_key,
            lock,
        },
        made,
    ))
}
