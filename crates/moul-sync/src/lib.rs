#![doc = include_str!("../README.md")]

#[cfg(not(target_arch = "wasm32"))]
mod dir;
mod error;
pub mod layout;
mod local;
pub mod maintain;
mod memory;
mod replica;
pub mod restore;
pub mod s3;
mod sync;
mod time;

#[cfg(not(target_arch = "wasm32"))]
pub use dir::DirReplica;
pub use error::{Error, ErrorKind, Result};
pub use local::{
    ConflictCopy, DeviceState, Local, Role, StoreLocal, store_apply, store_get, store_has_data,
    store_load_state, store_save_state, store_set,
};
pub use maintain::{Report, Retention};
pub use memory::{Fault, MemoryReplica};
#[cfg(not(target_arch = "wasm32"))]
pub use replica::block_on;
pub use replica::{BoxFuture, Condition, Object, ObjectInfo, Put, Replica};
pub use s3::{S3Config, S3Replica};
pub use sync::{Options, Status, Sync, new_id};
pub use time::{parse_rfc3339, rfc3339};
