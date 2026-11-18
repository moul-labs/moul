//! Leader election between tabs, with the Web Locks API.
//!
//! Opening an OPFS file for synchronous access takes an exclusive lock, so
//! only one worker per site can hold the database. Each tab's worker asks for
//! the same named Web Lock; the browser grants it to one of them at a time,
//! queues the rest, and releases it when the holder lets go or its tab closes.
//! The holder is the leader.

use std::fmt;

use js_sys::Function;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;

use crate::js;

/// A held Web Lock. Dropping it releases the lock, and the next waiting tab
/// becomes leader.
pub struct Leadership {
    name: String,
    release: Function,
    _callback: Closure<dyn FnMut(JsValue) -> JsValue>,
}

impl fmt::Debug for Leadership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Leadership")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Leadership {
    /// Waits until this context holds the lock `name`.
    pub async fn acquire(name: &str) -> Result<Leadership, JsValue> {
        Self::request(name, false)
            .await?
            .ok_or_else(|| js::error("the lock was not granted"))
    }

    /// Takes the lock `name` if nobody holds it, without waiting.
    pub async fn try_acquire(name: &str) -> Result<Option<Leadership>, JsValue> {
        Self::request(name, true).await
    }

    /// The lock's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    async fn request(name: &str, if_available: bool) -> Result<Option<Leadership>, JsValue> {
        let locks = js::get(&js::navigator(), "locks");
        if locks.is_undefined() {
            return Err(js::error("the Web Locks API is not available"));
        }
        let granted = js::deferred();
        let held = js::deferred();
        let held_promise = held.promise.clone();
        let on_grant = granted.resolve.clone();
        // The browser calls this with the lock (or null if `ifAvailable` found
        // it taken) and keeps the lock until the returned promise settles.
        let callback = Closure::<dyn FnMut(JsValue) -> JsValue>::new(move |lock: JsValue| {
            let _ = on_grant.call1(&JsValue::NULL, &lock);
            if lock.is_null() {
                JsValue::UNDEFINED
            } else {
                held_promise.clone().into()
            }
        });
        let options = js::object(&[("ifAvailable", JsValue::from_bool(if_available))]);
        let done = js::call(
            &locks,
            "request",
            &[JsValue::from_str(name), options, callback.as_ref().clone()],
        )?;
        // A failed request rejects this promise; surface it through `granted`.
        let reject = granted.reject.clone();
        let on_fail = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
            let _ = reject.call1(&JsValue::NULL, &e);
        });
        let _ = js::call(&done, "catch", &[on_fail.as_ref().clone()]);
        on_fail.forget();

        let lock = JsFuture::from(granted.promise).await?;
        if lock.is_null() {
            return Ok(None);
        }
        Ok(Some(Leadership {
            name: name.to_owned(),
            release: held.resolve,
            _callback: callback,
        }))
    }
}

impl Drop for Leadership {
    fn drop(&mut self) {
        let _ = self.release.call0(&JsValue::NULL);
    }
}

/// The lock name for the database called `db`.
pub fn lock_name(db: &str) -> String {
    format!("moul:{db}")
}
