#![doc = include_str!("../README.md")]
// Handlers return an HTTP response as their error, as axum does.
#![allow(clippy::result_large_err)]

// The server reads files and listens on sockets, so it builds only for
// native targets; the browser build of the workspace gets an empty crate.
#[cfg(not(target_arch = "wasm32"))]
pub mod auth_server;
#[cfg(not(target_arch = "wasm32"))]
pub mod chores;
#[cfg(not(target_arch = "wasm32"))]
pub mod config;
#[cfg(not(target_arch = "wasm32"))]
pub mod db;
#[cfg(not(target_arch = "wasm32"))]
pub mod docs;
#[cfg(not(target_arch = "wasm32"))]
pub mod effects;
#[cfg(not(target_arch = "wasm32"))]
pub mod http;
#[cfg(not(target_arch = "wasm32"))]
pub mod limits;
#[cfg(not(target_arch = "wasm32"))]
pub mod logs;
#[cfg(not(target_arch = "wasm32"))]
pub mod mail;
#[cfg(not(target_arch = "wasm32"))]
pub mod mcp;
#[cfg(not(target_arch = "wasm32"))]
pub mod net;
#[cfg(not(target_arch = "wasm32"))]
pub mod oauth;
#[cfg(not(target_arch = "wasm32"))]
pub mod open;
#[cfg(not(target_arch = "wasm32"))]
pub mod realtime;
#[cfg(not(target_arch = "wasm32"))]
pub mod replica;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;
#[cfg(not(target_arch = "wasm32"))]
pub mod webhooks;

#[cfg(not(target_arch = "wasm32"))]
pub use config::ServerConfig;
#[cfg(not(target_arch = "wasm32"))]
pub use server::{Running, start};
