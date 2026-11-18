//! Asking the browser to keep the site's storage.
//!
//! Browser storage is best-effort: under storage pressure, or in Safari after
//! seven days without a visit, it can be removed. A site that is granted
//! persistence is kept until the person clears it. Moul asks on the first
//! write. `persist()` exists only on the page, not in workers, so the worker
//! posts [`PERSIST_MESSAGE`] to its page, which calls [`request_persistence`].

use wasm_bindgen::prelude::*;

use crate::js;

/// The `type` of the message a worker posts to its page to ask it to request
/// persistent storage: `{ type: "moul:persist" }`.
pub const PERSIST_MESSAGE: &str = "moul:persist";

/// Whether the browser keeps this site's storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Persistence {
    /// The storage stays until the person clears it.
    Persistent,
    /// The browser may remove the storage under pressure.
    BestEffort,
    /// This context cannot tell or cannot ask, for example a private window.
    Unknown,
}

fn storage() -> Option<JsValue> {
    let s = js::get(&js::navigator(), "storage");
    (!s.is_undefined()).then_some(s)
}

/// Whether storage is persistent now. Works in pages and workers.
pub async fn persisted() -> Persistence {
    let Some(storage) = storage() else {
        return Persistence::Unknown;
    };
    match js::call_async(&storage, "persisted", &[]).await {
        Ok(v) if v.as_bool() == Some(true) => Persistence::Persistent,
        Ok(_) => Persistence::BestEffort,
        Err(_) => Persistence::Unknown,
    }
}

/// Asks for persistent storage. Only pages can ask; in a worker this returns
/// [`Persistence::Unknown`]. Firefox shows a prompt; Chrome decides from site
/// engagement; Safari grants it to apps added to the Home Screen.
pub async fn request() -> Persistence {
    let Some(storage) = storage() else {
        return Persistence::Unknown;
    };
    if js::get(&storage, "persist").is_undefined() {
        return Persistence::Unknown;
    }
    match js::call_async(&storage, "persist", &[]).await {
        Ok(v) if v.as_bool() == Some(true) => Persistence::Persistent,
        Ok(_) => Persistence::BestEffort,
        Err(_) => Persistence::Unknown,
    }
}

/// Bytes used and the quota, as the browser estimates them.
pub async fn estimate() -> Option<(f64, f64)> {
    let storage = storage()?;
    let e = js::call_async(&storage, "estimate", &[]).await.ok()?;
    Some((
        js::get(&e, "usage").as_f64()?,
        js::get(&e, "quota").as_f64()?,
    ))
}

/// For the page: asks for persistent storage and resolves to whether it is
/// granted. Call it when the worker posts `{ type: "moul:persist" }`.
#[wasm_bindgen(js_name = requestPersistence)]
pub async fn request_persistence() -> bool {
    request().await == Persistence::Persistent
}
