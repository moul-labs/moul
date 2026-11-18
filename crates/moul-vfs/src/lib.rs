#![doc = include_str!("../README.md")]

mod capture;
#[cfg(not(target_arch = "wasm32"))]
mod disk;
mod error;
mod log;
mod memory;
#[cfg(target_arch = "wasm32")]
mod opfs;
mod sqlite;
mod storage;
mod volume;

pub use capture::{Capture, Clock, Position, STATE_NAME, system_clock};
#[cfg(not(target_arch = "wasm32"))]
pub use disk::DiskStorage;
pub use error::{Error, ErrorKind, Result};
pub use log::{Commit, CommitSeq, FileCommit};
pub use memory::{CrashMode, MemoryStorage};
pub use moul_ltx::Crc64;
#[cfg(target_arch = "wasm32")]
pub use opfs::OpfsStorage;
pub use storage::{MaybeSend, Storage, StorageFile};
pub use volume::{Before, LOG_NAME, Observer, Volume};

/// The page size Moul uses for every database file. SQLite's native builds
/// default to it; the browser build defaults to 8,192, so stores set it.
pub const PAGE_SIZE: u32 = 4096;

// The browser test binaries must link SQLite for the VFS glue's symbols.
#[cfg(all(test, target_arch = "wasm32"))]
use sqlite_wasm_rs as _;
