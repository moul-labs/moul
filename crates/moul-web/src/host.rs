//! Opening the database in the leading tab and answering requests.
//!
//! The requests here are Phase 1's: raw SQL, export, import and status. They
//! exist to exercise storage end to end. Phase 4 replaces them with named
//! requests for records and the app definition.
//!
//! | `op`     | Fields            | Answer                                         |
//! | -------- | ----------------- | ---------------------------------------------- |
//! | `exec`   | `sql`, `params`?  | `{ changes, lastCommit }`                      |
//! | `query`  | `sql`, `params`?  | `{ columns, rows }`                            |
//! | `export` |                   | `Uint8Array` of `main.db`                      |
//! | `import` | `bytes`           | `{ lastCommit }`                               |
//! | `status` |                   | `{ durable, lastCommit, storage }`             |

use js_sys::{Array, Uint8Array};
use moul_store::{FILES, Store};
use moul_vfs::{OpfsStorage, Volume};
use rusqlite::types::{Value, ValueRef};
use wasm_bindgen::{JsCast, JsValue};

use crate::js;

/// Where a database's files live in OPFS.
pub fn opfs_path(db: &str) -> String {
    format!("moul/{db}")
}

/// A database opened by the leading tab.
pub struct Opened {
    /// The stores.
    pub store: Store,
    /// Whether the data survives the tab closing. `false` in a private window
    /// or any browser without OPFS: the engine then runs in memory.
    pub durable: bool,
    /// Why the data is not durable, when it is not.
    pub note: Option<String>,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("durable", &self.durable)
            .field("note", &self.note)
            .finish_non_exhaustive()
    }
}

fn opfs_unsupported(message: &str) -> bool {
    [
        "SecurityError",
        "NotAllowedError",
        "NotSupportedError",
        "needs a dedicated worker",
    ]
    .iter()
    .any(|m| message.contains(m))
}

/// Opens the database `db`: on OPFS when the browser offers it, in memory
/// otherwise. Call it only while holding the leader lock.
pub async fn open(db: &str) -> Result<Opened, JsValue> {
    let storage = js::get(&js::navigator(), "storage");
    let has_opfs = !storage.is_undefined() && !js::get(&storage, "getDirectory").is_undefined();
    let not_durable = |why: String| -> Result<Opened, JsValue> {
        Ok(Opened {
            store: Store::open_in_memory().map_err(|e| js::error(&e.to_string()))?,
            durable: false,
            note: Some(why),
        })
    };
    if !has_opfs {
        return not_durable("this browser has no origin private file system".into());
    }
    match OpfsStorage::open(&opfs_path(db), &FILES).await {
        Ok(storage) => {
            let volume = Volume::open(storage).map_err(|e| js::error(&e.to_string()))?;
            let store = Store::open(volume).map_err(|e| js::error(&e.to_string()))?;
            Ok(Opened {
                store,
                durable: true,
                note: None,
            })
        }
        Err(e) if opfs_unsupported(e.message()) => not_durable(format!(
            "the origin private file system is not available here ({})",
            e.message()
        )),
        Err(e) => Err(js::error(&e.to_string())),
    }
}

/// Answers requests against an opened database.
pub struct Engine {
    opened: Opened,
    first_write: Option<Box<dyn FnOnce()>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("opened", &self.opened)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Serves `opened`. `first_write` runs once, after the first request that
    /// commits; the worker uses it to ask the page for persistent storage.
    pub fn new(opened: Opened, first_write: Option<Box<dyn FnOnce()>>) -> Engine {
        Engine {
            opened,
            first_write,
        }
    }

    /// The stores.
    pub fn store(&self) -> &Store {
        &self.opened.store
    }

    /// Answers one request.
    pub fn handle(&mut self, req: JsValue) -> Result<JsValue, JsValue> {
        let before = self.opened.store.volume().last_commit();
        let answer = self.answer(&req);
        if self.opened.store.volume().last_commit() != before
            && let Some(f) = self.first_write.take()
        {
            f();
        }
        answer
    }

    fn answer(&mut self, req: &JsValue) -> Result<JsValue, JsValue> {
        let op = js::get_str(req, "op").ok_or_else(|| js::error("request has no op"))?;
        let store = &mut self.opened.store;
        let err = |e: &dyn std::fmt::Display| js::error(&e.to_string());
        match op.as_str() {
            "exec" => {
                let sql = js::get_str(req, "sql").ok_or_else(|| js::error("exec needs sql"))?;
                let params = params(&js::get(req, "params"))?;
                let changes = if params.is_empty() {
                    store.conn().execute_batch(&sql).map_err(|e| err(&e))?;
                    store.conn().changes()
                } else {
                    store
                        .conn()
                        .execute(&sql, rusqlite::params_from_iter(params))
                        .map_err(|e| err(&e))? as u64
                };
                Ok(js::object(&[
                    ("changes", (changes as f64).into()),
                    ("lastCommit", (store.volume().last_commit().0 as f64).into()),
                ]))
            }
            "query" => {
                let sql = js::get_str(req, "sql").ok_or_else(|| js::error("query needs sql"))?;
                let params = params(&js::get(req, "params"))?;
                query(store, &sql, params).map_err(|e| err(&e))
            }
            "export" => {
                let bytes = store.export().map_err(|e| err(&e))?;
                Ok(Uint8Array::from(bytes.as_slice()).into())
            }
            "import" => {
                let bytes: Uint8Array = js::get(req, "bytes")
                    .dyn_into()
                    .map_err(|_| js::error("import needs bytes as a Uint8Array"))?;
                let seq = store.import(&bytes.to_vec()).map_err(|e| err(&e))?;
                Ok(js::object(&[("lastCommit", (seq.0 as f64).into())]))
            }
            "status" => Ok(js::object(&[
                ("durable", self.opened.durable.into()),
                (
                    "note",
                    self.opened
                        .note
                        .clone()
                        .map_or(JsValue::NULL, JsValue::from),
                ),
                ("lastCommit", (store.volume().last_commit().0 as f64).into()),
            ])),
            other => Err(js::error(&format!("unknown op {other:?}"))),
        }
    }
}

fn params(v: &JsValue) -> Result<Vec<Value>, JsValue> {
    if v.is_undefined() || v.is_null() {
        return Ok(Vec::new());
    }
    let arr: Array = v
        .clone()
        .dyn_into()
        .map_err(|_| js::error("params must be an array"))?;
    arr.iter().map(|p| to_value(&p)).collect()
}

fn to_value(v: &JsValue) -> Result<Value, JsValue> {
    if v.is_null() || v.is_undefined() {
        return Ok(Value::Null);
    }
    if let Some(b) = v.as_bool() {
        return Ok(Value::Integer(b as i64));
    }
    if let Some(n) = v.as_f64() {
        let safe = n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0;
        return Ok(if safe {
            Value::Integer(n as i64)
        } else {
            Value::Real(n)
        });
    }
    if v.is_bigint() {
        let s = js_sys::BigInt::from(v.clone())
            .to_string(10)
            .map_err(|_| js::error("bad bigint"))?;
        let s: String = s.into();
        return s
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| js::error("bigint out of range for SQLite"));
    }
    if let Some(s) = v.as_string() {
        return Ok(Value::Text(s));
    }
    if let Some(bytes) = v.dyn_ref::<Uint8Array>() {
        return Ok(Value::Blob(bytes.to_vec()));
    }
    Err(js::error("unsupported parameter type"))
}

fn to_js(v: ValueRef<'_>) -> JsValue {
    match v {
        ValueRef::Null => JsValue::NULL,
        ValueRef::Integer(i) if i.unsigned_abs() <= 9_007_199_254_740_991 => (i as f64).into(),
        ValueRef::Integer(i) => js_sys::BigInt::from(i).into(),
        ValueRef::Real(f) => f.into(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).as_ref().into(),
        ValueRef::Blob(b) => Uint8Array::from(b).into(),
    }
}

fn query(store: &Store, sql: &str, params: Vec<Value>) -> rusqlite::Result<JsValue> {
    let mut stmt = store.conn().prepare(sql)?;
    let columns: Array = stmt.column_names().map(JsValue::from).collect();
    let n = stmt.column_count();
    let rows = Array::new();
    let mut it = stmt.query(rusqlite::params_from_iter(params))?;
    while let Some(row) = it.next()? {
        let out = Array::new();
        for i in 0..n {
            out.push(&to_js(row.get_ref(i)?));
        }
        rows.push(&out);
    }
    Ok(js::object(&[
        ("columns", columns.into()),
        ("rows", rows.into()),
    ]))
}
