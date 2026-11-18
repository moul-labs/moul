#![doc = include_str!("../README.md")]
//!
//! # Pieces
//!
//! - `leader`: one tab at a time owns the database files, chosen with the
//!   Web Locks API.
//! - `tabs`: every tab's requests reach that leader over a broadcast channel.
//! - `persist`: asking the browser to keep the site's storage.
//! - `host`: opening the stores on OPFS, or in memory where OPFS is missing,
//!   and answering requests, sync included.
//! - `conflicts`: conflict copies kept in OPFS.
//! - `MoulWorker`: the entry point a dedicated worker starts.
//!
//! Everything here is browser-only; on native targets the crate is empty.

#[cfg(target_arch = "wasm32")]
mod js;

#[cfg(target_arch = "wasm32")]
pub mod conflicts;
#[cfg(target_arch = "wasm32")]
pub mod host;
#[cfg(target_arch = "wasm32")]
pub mod leader;
#[cfg(target_arch = "wasm32")]
pub mod persist;
#[cfg(target_arch = "wasm32")]
pub mod tabs;
#[cfg(target_arch = "wasm32")]
mod worker;

#[cfg(target_arch = "wasm32")]
pub use worker::MoulWorker;
