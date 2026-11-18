//! Small helpers over `js_sys`.

use js_sys::{Function, Object, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

/// A promise and the functions that settle it.
pub(crate) struct Deferred {
    pub promise: Promise,
    pub resolve: Function,
    pub reject: Function,
}

pub(crate) fn deferred() -> Deferred {
    let mut parts = None;
    let promise = Promise::new(&mut |resolve, reject| parts = Some((resolve, reject)));
    let (resolve, reject) = parts.expect("the executor runs synchronously");
    Deferred {
        promise,
        resolve,
        reject,
    }
}

pub(crate) fn get(target: &JsValue, key: &str) -> JsValue {
    Reflect::get(target, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

pub(crate) fn get_str(target: &JsValue, key: &str) -> Option<String> {
    get(target, key).as_string()
}

/// Builds a plain object from key-value pairs.
pub(crate) fn object(pairs: &[(&str, JsValue)]) -> JsValue {
    let obj = Object::new();
    for (k, v) in pairs {
        let _ = Reflect::set(&obj, &JsValue::from_str(k), v);
    }
    obj.into()
}

/// Calls `target[method](...args)`.
pub(crate) fn call(target: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let f: Function = get(target, method)
        .dyn_into()
        .map_err(|_| error(&format!("{method} is not available here")))?;
    let args: js_sys::Array = args.iter().collect();
    f.apply(target, &args)
}

pub(crate) async fn call_async(
    target: &JsValue,
    method: &str,
    args: &[JsValue],
) -> Result<JsValue, JsValue> {
    let p: Promise = call(target, method, args)?
        .dyn_into()
        .map_err(|_| error(&format!("{method} did not return a promise")))?;
    JsFuture::from(p).await
}

/// The global object: a window or a worker scope.
pub(crate) fn global() -> JsValue {
    js_sys::global().into()
}

/// `navigator` in a window or a worker.
pub(crate) fn navigator() -> JsValue {
    get(&js_sys::global(), "navigator")
}

/// A JavaScript `Error` with `message`.
pub(crate) fn error(message: &str) -> JsValue {
    js_sys::Error::new(message).into()
}

/// The message of a JavaScript error, or its debug form.
pub(crate) fn message(err: &JsValue) -> String {
    get_str(err, "message").unwrap_or_else(|| format!("{err:?}"))
}

pub(crate) async fn sleep_ms(ms: u32) {
    let d = deferred();
    let _ = call(
        &js_sys::global(),
        "setTimeout",
        &[d.resolve.into(), ms.into()],
    );
    let _ = JsFuture::from(d.promise).await;
}
