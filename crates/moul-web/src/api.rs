//! Engine requests and responses as JavaScript values.
//!
//! The page sends the engine the same requests a hub would get over HTTP,
//! as plain objects that survive `postMessage`:
//!
//! ```js
//! {
//!   op: "api",
//!   method: "GET",                          // default "GET"
//!   path: "/api/moul/posts/records",
//!   query: "filter=..." | [["k", "v"]] | { k: "v" },
//!   headers: [["Authorization", "Bearer …"]] | { Authorization: "…" },
//!   json: { … },                            // a JSON body, or
//!   form: { fields: [["k", "v"]], files: [["field", { name, type, data: Uint8Array }]] },
//!   bytes: Uint8Array, type: "text/csv",    // a raw body
//!   owner: true,                            // the builder: rules do not apply
//!   id: "device:7",                         // the request id of a write
//! }
//! ```
//!
//! The answer is `{ status, headers: [["k", "v"]], json }` for a JSON body,
//! `{ status, headers, bytes, type }` for bytes, or `{ status, headers }`.

use js_sys::{Array, Object, Uint8Array};
use moul_core::{Body, Caller, Request, Response, ResponseBody, Subscription, Upload};
use wasm_bindgen::{JsCast, JsValue};

use crate::js;

/// Reads `v` as a list of string pairs: `[["k", "v"]]`, or an object's
/// own entries.
fn pairs(v: &JsValue, what: &str) -> Result<Vec<(String, String)>, JsValue> {
    if v.is_undefined() || v.is_null() {
        return Ok(Vec::new());
    }
    let entries: Array = if Array::is_array(v) {
        v.clone().unchecked_into()
    } else if v.is_object() {
        Object::entries(v.unchecked_ref())
    } else {
        return Err(js::error(&format!(
            "{what} must be an array of pairs or an object"
        )));
    };
    entries
        .iter()
        .map(|pair| {
            let k = js_sys::Reflect::get_u32(&pair, 0)
                .ok()
                .and_then(|k| k.as_string());
            let v = js_sys::Reflect::get_u32(&pair, 1)
                .ok()
                .and_then(|v| v.as_string());
            match (k, v) {
                (Some(k), Some(v)) => Ok((k, v)),
                _ => Err(js::error(&format!("{what} must hold string pairs"))),
            }
        })
        .collect()
}

/// A JavaScript value as JSON, through `JSON.stringify`. `undefined` (and
/// anything else `JSON.stringify` drops) is `None`.
pub(crate) fn to_json(v: &JsValue) -> Result<Option<serde_json::Value>, JsValue> {
    if v.is_undefined() {
        return Ok(None);
    }
    let text = js_sys::JSON::stringify(v)?;
    let Some(text) = text.as_string() else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| js::error(&format!("the body is not JSON: {e}")))
}

/// A JSON value as a JavaScript value.
pub(crate) fn from_json(v: &serde_json::Value) -> JsValue {
    let text = serde_json::to_string(v).expect("JSON values serialize");
    js_sys::JSON::parse(&text).unwrap_or(JsValue::NULL)
}

fn bytes_of(v: &JsValue, what: &str) -> Result<Vec<u8>, JsValue> {
    v.dyn_ref::<Uint8Array>()
        .map(Uint8Array::to_vec)
        .ok_or_else(|| js::error(&format!("{what} must be a Uint8Array")))
}

fn body(req: &JsValue) -> Result<Body, JsValue> {
    let json = js::get(req, "json");
    if !json.is_undefined() {
        return Ok(to_json(&json)?.map_or(Body::None, Body::Json));
    }
    let form = js::get(req, "form");
    if !form.is_undefined() && !form.is_null() {
        let fields = pairs(&js::get(&form, "fields"), "form.fields")?;
        let mut files = Vec::new();
        let list = js::get(&form, "files");
        if !list.is_undefined() && !list.is_null() {
            let list: Array = list
                .dyn_into()
                .map_err(|_| js::error("form.files must be an array of [field, file] pairs"))?;
            for pair in list.iter() {
                let field = js_sys::Reflect::get_u32(&pair, 0)?
                    .as_string()
                    .ok_or_else(|| js::error("form.files: a field name must be a string"))?;
                let file = js_sys::Reflect::get_u32(&pair, 1)?;
                files.push((
                    field,
                    Upload {
                        name: js::get_str(&file, "name").unwrap_or_default(),
                        content_type: js::get_str(&file, "type").unwrap_or_default(),
                        data: bytes_of(&js::get(&file, "data"), "form.files: data")?,
                    },
                ));
            }
        }
        return Ok(Body::Form { fields, files });
    }
    let bytes = js::get(req, "bytes");
    if !bytes.is_undefined() && !bytes.is_null() {
        return Ok(Body::Bytes {
            content_type: js::get_str(req, "type").unwrap_or_default(),
            data: bytes_of(&bytes, "bytes")?,
        });
    }
    Ok(Body::None)
}

/// Reads an `api` request.
pub fn to_request(req: &JsValue) -> Result<Request, JsValue> {
    let method = js::get_str(req, "method").unwrap_or_else(|| "GET".into());
    let path = js::get_str(req, "path").ok_or_else(|| js::error("an api request needs a path"))?;
    if !path.starts_with('/') {
        return Err(js::error("an api request's path must start with /"));
    }
    // A path may carry its query, as a URL would.
    let (path, inline_query) = match path.split_once('?') {
        Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
        None => (path, None),
    };
    let mut r = Request::new(&method, &path);
    if let Some(q) = inline_query {
        r.query = moul_core::api::parse_query(&q);
    }
    let query = js::get(req, "query");
    if let Some(q) = query.as_string() {
        r.query
            .extend(moul_core::api::parse_query(q.trim_start_matches('?')));
    } else {
        r.query.extend(pairs(&query, "query")?);
    }
    r.headers = pairs(&js::get(req, "headers"), "headers")?;
    r.body = body(req)?;
    if js::get(req, "owner").as_bool() == Some(true) {
        r.caller = Caller::Owner;
    }
    r.id = js::get_str(req, "id").filter(|s| !s.is_empty());
    Ok(r)
}

/// Writes a response for the page.
pub fn response_to_js(r: &Response) -> JsValue {
    let headers: Array = r
        .headers
        .iter()
        .map(|(k, v)| -> JsValue {
            let pair = Array::new();
            pair.push(&k.into());
            pair.push(&v.into());
            pair.into()
        })
        .collect();
    let mut out = vec![
        ("status", JsValue::from(r.status)),
        ("headers", headers.into()),
    ];
    match &r.body {
        ResponseBody::Empty => {}
        ResponseBody::Json(v) => out.push(("json", from_json(v))),
        ResponseBody::Bytes { content_type, data } => {
            out.push(("bytes", Uint8Array::from(data.as_slice()).into()));
            out.push(("type", content_type.as_str().into()));
        }
    }
    js::object(&out)
}

/// Reads a `subscribe` request: `{ collection, record?, events?, headers?,
/// owner? }`.
pub fn to_subscription(req: &JsValue) -> Result<Subscription, JsValue> {
    let collection = js::get_str(req, "collection")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| js::error("subscribe needs a collection, or \"*\""))?;
    let events = js::get(req, "events");
    let events = if events.is_undefined() || events.is_null() {
        Vec::new()
    } else {
        let list: Array = events
            .dyn_into()
            .map_err(|_| js::error("subscribe: events must be an array of strings"))?;
        list.iter().filter_map(|e| e.as_string()).collect()
    };
    Ok(Subscription {
        collection,
        record: js::get_str(req, "record").filter(|r| !r.is_empty()),
        events,
        credentials: pairs(&js::get(req, "headers"), "headers")?,
        owner: js::get(req, "owner").as_bool() == Some(true),
    })
}

/// The ids in a `visible` list, if any.
pub fn visible(req: &JsValue) -> Vec<String> {
    let v = js::get(req, "visible");
    if !Array::is_array(&v) {
        return Vec::new();
    }
    Array::from(&v)
        .iter()
        .filter_map(|id| id.as_string())
        .collect()
}
