//! Conflict copies in the origin private file system.
//!
//! When sync must replace the local database while it holds changes the
//! replica does not, the old database is kept here first, one SQLite file
//! each, in `moul/<db>-conflicts/`. The page lists them, offers them for
//! download, and deletes them when the builder is done.

use js_sys::{Array, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};

use crate::js;

/// What a database's conflict folder adds to its name. No database may be
/// named so, or it would share another's conflict folder.
pub(crate) const SUFFIX: &str = "-conflicts";

fn folder_path(db: &str) -> String {
    format!("moul/{db}{SUFFIX}")
}

/// Opens (and creates) the folder at a `/`-separated path under the OPFS root.
async fn folder(path: &str) -> Result<JsValue, JsValue> {
    let storage = js::get(&js::navigator(), "storage");
    let mut dir = js::call_async(&storage, "getDirectory", &[]).await?;
    let create = js::object(&[("create", true.into())]);
    for part in path.split('/').filter(|p| !p.is_empty()) {
        dir = js::call_async(&dir, "getDirectoryHandle", &[part.into(), create.clone()]).await?;
    }
    Ok(dir)
}

fn check(name: &str) -> Result<(), JsValue> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(js::error(&format!("{name:?} is not a conflict copy")));
    }
    Ok(())
}

/// Keeps `bytes` as `name`, flushed.
pub async fn save(db: &str, name: &str, bytes: &[u8]) -> Result<(), JsValue> {
    check(name)?;
    let dir = folder(&folder_path(db)).await?;
    let create = js::object(&[("create", true.into())]);
    let file = js::call_async(&dir, "getFileHandle", &[name.into(), create]).await?;
    // Sync access handles exist in every engine's dedicated workers.
    let handle = js::call_async(&file, "createSyncAccessHandle", &[]).await?;
    let written = (|| {
        js::call(&handle, "truncate", &[0.into()])?;
        let data = Uint8Array::from(bytes);
        let at = js::object(&[("at", 0.into())]);
        js::call(&handle, "write", &[data.into(), at])?;
        js::call(&handle, "flush", &[])
    })();
    let _ = js::call(&handle, "close", &[]);
    written?;
    Ok(())
}

/// The names of the copies kept for `db`, oldest first.
pub async fn list(db: &str) -> Result<Vec<String>, JsValue> {
    let dir = folder(&folder_path(db)).await?;
    let iter = js::call(&dir, "keys", &[])?;
    let mut names = Vec::new();
    loop {
        let next = js::call(&iter, "next", &[])?;
        let step = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(next)).await?;
        if js::get(&step, "done").as_bool() == Some(true) {
            break;
        }
        if let Some(name) = js::get(&step, "value").as_string()
            && !name.starts_with('.')
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// The bytes of copy `name`.
pub async fn read(db: &str, name: &str) -> Result<Uint8Array, JsValue> {
    check(name)?;
    let dir = folder(&folder_path(db)).await?;
    let file = js::call_async(&dir, "getFileHandle", &[name.into()]).await?;
    let blob = js::call_async(&file, "getFile", &[]).await?;
    let buf = js::call_async(&blob, "arrayBuffer", &[]).await?;
    Ok(Uint8Array::new(&buf))
}

/// Deletes copy `name`.
pub async fn delete(db: &str, name: &str) -> Result<(), JsValue> {
    check(name)?;
    let dir = folder(&folder_path(db)).await?;
    js::call_async(&dir, "removeEntry", &[name.into()]).await?;
    Ok(())
}

/// `names` as a JavaScript array.
pub fn to_array(names: &[String]) -> JsValue {
    names
        .iter()
        .map(|n| JsValue::from_str(n))
        .collect::<Array>()
        .unchecked_into()
}
