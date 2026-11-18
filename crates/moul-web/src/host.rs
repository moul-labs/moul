//! Opening the database in the leading tab and answering requests.
//!
//! The SQL requests are Phase 1's: raw SQL, export, import and status. They
//! exist to exercise storage end to end. Phase 4 replaces them with named
//! requests for records and the app definition. The sync requests are Phase
//! 3's and stay.
//!
//! | `op`              | Fields              | Answer                                 |
//! | ----------------- | ------------------- | -------------------------------------- |
//! | `exec`            | `sql`, `params`?    | `{ changes, lastCommit }`              |
//! | `query`           | `sql`, `params`?    | `{ columns, rows }`                    |
//! | `export`          |                     | `Uint8Array` of `main.db`              |
//! | `import`          | `bytes`             | `{ lastCommit }`                       |
//! | `status`          |                     | `{ durable, lastCommit, storage }`     |
//! | `syncConfigure`   | `bucket`, …         | sync status, after connecting          |
//! | `syncStatus`      |                     | sync status                            |
//! | `syncNow`         |                     | sync status, after one round           |
//! | `takeOver`        |                     | sync status, after taking over writing |
//! | `syncDisconnect`  |                     | sync status                            |
//! | `conflicts`       |                     | names of the conflict copies kept      |
//! | `conflictExport`  | `name`              | `Uint8Array` of the copy               |
//! | `conflictDelete`  | `name`              | `{}`                                   |
//!
//! `syncConfigure` takes `endpoint`, `bucket`, `accessKeyId` and
//! `secretAccessKey`, and optionally `region`, `prefix`, `pathStyle` and
//! `label`. The settings, keys included, stay in this device's own store and
//! never reach the replica. The sync requests answer with a promise.
//!
//! While the device follows another writer, the shared store is read-only:
//! `exec` runs with `PRAGMA query_only` and `import` is refused.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::{Array, Uint8Array};
use moul_store::{FILES, Store};
use moul_sync::{
    BoxFuture, ConflictCopy, DeviceState, Local, Role, S3Config, S3Replica, Status, Sync,
    store_apply, store_get, store_has_data, store_load_state, store_save_state, store_set,
};
use moul_vfs::{
    ApplyMode, Capture, MemoryStorage, OpfsStorage, PACK_NAME, Position, STATE_NAME, Volume,
};
use rusqlite::types::{Value, ValueRef};
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::future_to_promise;

use crate::{conflicts, js};

/// Where a database's files live in OPFS.
pub fn opfs_path(db: &str) -> String {
    format!("moul/{db}")
}

/// A database opened by the leading tab.
pub struct Opened {
    /// The database name.
    pub db: String,
    /// The stores.
    pub store: Store,
    /// The capture of the shared store, which sync pushes from.
    pub capture: Capture,
    /// Whether the data survives the tab closing. `false` in a private window
    /// or any browser without OPFS: the engine then runs in memory.
    pub durable: bool,
    /// Why the data is not durable, when it is not.
    pub note: Option<String>,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("db", &self.db)
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

fn err(e: &dyn std::fmt::Display) -> JsValue {
    js::error(&e.to_string())
}

/// Opens the stores on `volume`. A device that follows a replica must never
/// write to its shared store, so it opens as a follower; any other marks a
/// plain imported file as a shared store, as Phase 1 did.
fn open_stores(volume: Volume) -> Result<Store, JsValue> {
    let store = Store::open_follower(volume).map_err(|e| err(&e))?;
    let follower = store_load_state(&store)
        .ok()
        .flatten()
        .is_some_and(|s| s.role == Role::Follower);
    if follower {
        return Ok(store);
    }
    let volume = store.close().map_err(|e| err(&e))?;
    Store::open(volume).map_err(|e| err(&e))
}

/// Opens the database `db`: on OPFS when the browser offers it, in memory
/// otherwise. Call it only while holding the leader lock.
pub async fn open(db: &str) -> Result<Opened, JsValue> {
    let storage = js::get(&js::navigator(), "storage");
    let has_opfs = !storage.is_undefined() && !js::get(&storage, "getDirectory").is_undefined();
    let not_durable = |why: String| -> Result<Opened, JsValue> {
        let capture = Capture::open(MemoryStorage::new(), "main.db").map_err(|e| err(&e))?;
        let volume =
            Volume::open_observed(MemoryStorage::new(), capture.clone()).map_err(|e| err(&e))?;
        Ok(Opened {
            db: db.to_owned(),
            store: open_stores(volume)?,
            capture,
            durable: false,
            note: Some(why),
        })
    };
    if !has_opfs {
        return not_durable("this browser has no origin private file system".into());
    }
    let opened = async {
        let storage = OpfsStorage::open(&opfs_path(db), &FILES).await?;
        let ltx =
            OpfsStorage::open(&format!("{}/ltx", opfs_path(db)), &[STATE_NAME, PACK_NAME]).await?;
        Ok::<_, moul_vfs::Error>((storage, ltx))
    }
    .await;
    match opened {
        Ok((storage, ltx)) => {
            let capture = Capture::open(ltx, "main.db").map_err(|e| err(&e))?;
            let volume = Volume::open_observed(storage, capture.clone()).map_err(|e| err(&e))?;
            Ok(Opened {
                db: db.to_owned(),
                store: open_stores(volume)?,
                capture,
                durable: true,
                note: None,
            })
        }
        Err(e) if opfs_unsupported(e.message()) => not_durable(format!(
            "the origin private file system is not available here ({})",
            e.message()
        )),
        Err(e) => Err(err(&e)),
    }
}

/// Sync settings, as `syncConfigure` takes them and the device store keeps
/// them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncSettings {
    endpoint: String,
    bucket: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    region: String,
    access_key_id: String,
    secret_access_key: String,
    #[serde(default)]
    path_style: Option<bool>,
    #[serde(default)]
    label: String,
}

impl SyncSettings {
    fn from_js(req: &JsValue) -> Result<Self, JsValue> {
        let text = |k: &str| js::get_str(req, k).unwrap_or_default().trim().to_owned();
        let s = SyncSettings {
            endpoint: text("endpoint").trim_end_matches('/').to_owned(),
            bucket: text("bucket"),
            prefix: text("prefix").trim_matches('/').to_owned(),
            region: text("region"),
            access_key_id: text("accessKeyId"),
            secret_access_key: text("secretAccessKey"),
            path_style: js::get(req, "pathStyle").as_bool(),
            label: text("label"),
        };
        for (name, v) in [
            ("endpoint", &s.endpoint),
            ("bucket", &s.bucket),
            ("accessKeyId", &s.access_key_id),
            ("secretAccessKey", &s.secret_access_key),
        ] {
            if v.is_empty() {
                return Err(js::error(&format!("syncConfigure needs {name}")));
            }
        }
        Ok(s)
    }

    fn replica(&self) -> Result<S3Replica, JsValue> {
        let region = if !self.region.is_empty() {
            self.region.clone()
        } else if self.endpoint.contains("r2.cloudflarestorage.com") {
            "auto".into()
        } else {
            "us-east-1".into()
        };
        S3Replica::with_default_client(S3Config {
            endpoint: self.endpoint.clone(),
            region,
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            session_token: None,
            path_style: self.path_style.unwrap_or(true),
        })
        .map_err(|e| err(&e))
    }

    fn sync(&self) -> Result<Sync, JsValue> {
        Ok(Sync::new(
            self.replica()?,
            moul_sync::Options {
                label: self.label.clone(),
                ..moul_sync::Options::default()
            },
        ))
    }
}

struct Db {
    store: Store,
    capture: Capture,
    durable: bool,
    note: Option<String>,
    /// The device follows another writer: no writes to the shared store.
    read_only: bool,
    first_write: Option<Box<dyn FnOnce()>>,
}

struct Inner {
    name: String,
    db: RefCell<Db>,
    /// The sync session. Taken out while a sync call runs, so requests in
    /// between can still use the database.
    sync: RefCell<Option<Sync>>,
    settings: RefCell<Option<SyncSettings>>,
    busy: Cell<bool>,
    status: RefCell<Option<Status>>,
    /// Conflict copies, when there is no OPFS to keep them in.
    kept: RefCell<Vec<(String, Vec<u8>)>>,
}

/// Answers requests against an opened database. Clones share it.
#[derive(Clone)]
pub struct Engine {
    inner: Rc<Inner>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("db", &self.inner.name)
            .finish_non_exhaustive()
    }
}

/// The engine's database as sync sees it, borrowed one call at a time.
struct EngineLocal(Engine);

fn sync_err(e: JsValue) -> moul_sync::Error {
    moul_sync::Error::new(moul_sync::ErrorKind::Local, js::message(&e))
}

impl Local for EngineLocal {
    fn capture(&self) -> Capture {
        self.0.inner.db.borrow().capture.clone()
    }

    fn apply(&mut self, ltx: &[u8], mode: ApplyMode) -> moul_sync::Result<Position> {
        let mut db = self.0.inner.db.borrow_mut();
        let capture = db.capture.clone();
        store_apply(&mut db.store, &capture, ltx, mode)
    }

    fn fence(&mut self) -> moul_sync::Result<moul_ltx::Txid> {
        let db = self.0.inner.db.borrow();
        Ok(db.capture.fence(db.store.volume())?)
    }

    fn rebase(&mut self) -> moul_sync::Result<Option<moul_ltx::Txid>> {
        let db = self.0.inner.db.borrow();
        Ok(db.capture.rebase(db.store.volume())?)
    }

    fn export(&mut self) -> moul_sync::Result<Vec<u8>> {
        Ok(self.0.inner.db.borrow().store.export()?)
    }

    fn has_data(&mut self) -> moul_sync::Result<bool> {
        store_has_data(&self.0.inner.db.borrow().store)
    }

    fn load_state(&mut self) -> moul_sync::Result<Option<DeviceState>> {
        store_load_state(&self.0.inner.db.borrow().store)
    }

    fn save_state(&mut self, state: &DeviceState) -> moul_sync::Result<()> {
        store_save_state(&self.0.inner.db.borrow().store, state)
    }

    fn save_conflict(&mut self, copy: ConflictCopy) -> BoxFuture<'_, moul_sync::Result<String>> {
        Box::pin(async move {
            let inner = &self.0.inner;
            if inner.db.borrow().durable {
                conflicts::save(&inner.name, &copy.name, &copy.bytes)
                    .await
                    .map_err(sync_err)
            } else {
                inner
                    .kept
                    .borrow_mut()
                    .push((copy.name.clone(), copy.bytes));
                Ok(format!("memory:{}", copy.name))
            }
        })
    }

    fn role_changed(&mut self, role: Role) {
        self.0.inner.db.borrow_mut().read_only = role == Role::Follower;
    }
}

/// What a sync call does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Connect,
    Round,
    Tick,
    TakeOver,
    Disconnect,
}

/// An engine that does not keep it open, for background work that should
/// stop once the engine is closed.
#[derive(Clone, Debug)]
pub struct WeakEngine(std::rc::Weak<Inner>);

impl WeakEngine {
    /// The engine, if it is still open.
    pub fn upgrade(&self) -> Option<Engine> {
        self.0.upgrade().map(|inner| Engine { inner })
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("db", &self.name)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// A handle that does not keep the engine open.
    pub fn downgrade(&self) -> WeakEngine {
        WeakEngine(Rc::downgrade(&self.inner))
    }

    /// Serves `opened`. `first_write` runs once, after the first request that
    /// commits; the worker uses it to ask the page for persistent storage.
    /// Sync starts if the device was set up for it before.
    pub fn new(opened: Opened, first_write: Option<Box<dyn FnOnce()>>) -> Engine {
        let settings = store_get(&opened.store, "config")
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<SyncSettings>(&json).ok());
        let engine = Engine {
            inner: Rc::new(Inner {
                name: opened.db,
                db: RefCell::new(Db {
                    store: opened.store,
                    capture: opened.capture,
                    durable: opened.durable,
                    note: opened.note,
                    read_only: false,
                    first_write,
                }),
                sync: RefCell::new(None),
                settings: RefCell::new(None),
                busy: Cell::new(false),
                status: RefCell::new(None),
                kept: RefCell::new(Vec::new()),
            }),
        };
        if let Some(s) = settings
            && let Ok(mut sync) = s.sync()
        {
            // Restores the role the device last had, so that a writer keeps
            // writing and a follower stays read-only while offline.
            let _ = sync.load(&mut EngineLocal(engine.clone()));
            *engine.inner.status.borrow_mut() =
                Some(sync.status(&engine.inner.db.borrow().capture));
            *engine.inner.sync.borrow_mut() = Some(sync);
            *engine.inner.settings.borrow_mut() = Some(s);
        }
        engine
    }

    /// The stores. Do not hold the borrow across an `await`.
    pub fn store(&self) -> std::cell::Ref<'_, Store> {
        std::cell::Ref::map(self.inner.db.borrow(), |db| &db.store)
    }

    /// Whether sync is set up.
    pub fn syncing(&self) -> bool {
        self.inner.settings.borrow().is_some()
    }

    /// Runs one round of background sync if one is due and none is running.
    /// The worker calls this about once a second.
    pub async fn tick(&self) {
        if self.inner.sync.borrow().is_some() && !self.inner.busy.get() {
            let _ = self.run(Action::Tick).await;
        }
    }

    /// Answers one request. Sync requests answer with a promise.
    pub fn handle(&self, req: JsValue) -> Result<JsValue, JsValue> {
        let op = js::get_str(&req, "op").ok_or_else(|| js::error("request has no op"))?;
        if let Some(answer) = self.handle_sync(&op, &req) {
            return answer;
        }
        let before = self.inner.db.borrow().store.volume().last_commit();
        let answer = self.answer(&op, &req);
        let mut db = self.inner.db.borrow_mut();
        if db.store.volume().last_commit() != before {
            if let Some(f) = db.first_write.take() {
                f();
            }
            // Without sync nothing reads the level-0 files: drop them.
            if self.inner.settings.borrow().is_none() {
                let at = db.capture.position().txid;
                let _ = db.capture.release(at);
            }
        }
        answer
    }

    fn answer(&self, op: &str, req: &JsValue) -> Result<JsValue, JsValue> {
        let mut db = self.inner.db.borrow_mut();
        let read_only = db.read_only;
        let store = &mut db.store;
        match op {
            "exec" => {
                let sql = js::get_str(req, "sql").ok_or_else(|| js::error("exec needs sql"))?;
                let params = params(&js::get(req, "params"))?;
                if read_only {
                    store
                        .conn()
                        .execute_batch("PRAGMA query_only = 1")
                        .map_err(|e| err(&e))?;
                }
                let run = || -> rusqlite::Result<u64> {
                    if params.is_empty() {
                        store.conn().execute_batch(&sql)?;
                        Ok(store.conn().changes())
                    } else {
                        Ok(store
                            .conn()
                            .execute(&sql, rusqlite::params_from_iter(params))?
                            as u64)
                    }
                };
                let result = run();
                if read_only {
                    let _ = store.conn().execute_batch("PRAGMA query_only = 0");
                }
                let changes = result.map_err(|e| {
                    if read_only && e.to_string().contains("readonly") {
                        js::error(
                            "this device follows another writer, so its data is read-only; \
                             take over writing to change it",
                        )
                    } else {
                        err(&e)
                    }
                })?;
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
                if read_only {
                    return Err(js::error(
                        "this device follows another writer; take over writing before importing",
                    ));
                }
                let bytes: Uint8Array = js::get(req, "bytes")
                    .dyn_into()
                    .map_err(|_| js::error("import needs bytes as a Uint8Array"))?;
                let seq = store.import(&bytes.to_vec()).map_err(|e| err(&e))?;
                Ok(js::object(&[("lastCommit", (seq.0 as f64).into())]))
            }
            "status" => Ok(js::object(&[
                ("durable", db.durable.into()),
                ("note", db.note.clone().map_or(JsValue::NULL, JsValue::from)),
                (
                    "lastCommit",
                    (db.store.volume().last_commit().0 as f64).into(),
                ),
                ("readOnly", read_only.into()),
            ])),
            other => Err(js::error(&format!("unknown op {other:?}"))),
        }
    }

    /// The sync requests, which answer with a promise. `None` for any other.
    fn handle_sync(&self, op: &str, req: &JsValue) -> Option<Result<JsValue, JsValue>> {
        let engine = self.clone();
        let req = req.clone();
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsValue, JsValue>>>> =
            match op {
                "syncStatus" => return Some(Ok(self.status_js())),
                "syncConfigure" => Box::pin(async move {
                    let settings = SyncSettings::from_js(&req)?;
                    let sync = settings.sync()?;
                    engine.stop_sync().await;
                    {
                        let db = engine.inner.db.borrow();
                        let json = serde_json::to_string(&settings).expect("settings serialize");
                        store_set(&db.store, "config", Some(&json)).map_err(|e| err(&e))?;
                    }
                    *engine.inner.settings.borrow_mut() = Some(settings);
                    *engine.inner.sync.borrow_mut() = Some(sync);
                    engine.run(Action::Connect).await?;
                    Ok(engine.status_js())
                }),
                "syncNow" => Box::pin(async move {
                    engine.run(Action::Round).await?;
                    Ok(engine.status_js())
                }),
                "takeOver" => Box::pin(async move {
                    engine.run(Action::TakeOver).await?;
                    Ok(engine.status_js())
                }),
                "syncDisconnect" => Box::pin(async move {
                    if engine.inner.sync.borrow().is_some() || engine.inner.busy.get() {
                        engine.run(Action::Disconnect).await?;
                    }
                    engine.stop_sync().await;
                    let db = engine.inner.db.borrow();
                    store_set(&db.store, "config", None).map_err(|e| err(&e))?;
                    drop(db);
                    *engine.inner.status.borrow_mut() = None;
                    Ok(engine.status_js())
                }),
                "conflicts" => Box::pin(async move {
                    let names = if engine.inner.db.borrow().durable {
                        conflicts::list(&engine.inner.name).await?
                    } else {
                        engine
                            .inner
                            .kept
                            .borrow()
                            .iter()
                            .map(|(n, _)| n.clone())
                            .collect()
                    };
                    Ok(conflicts::to_array(&names))
                }),
                "conflictExport" => Box::pin(async move {
                    let name = js::get_str(&req, "name")
                        .ok_or_else(|| js::error("conflictExport needs a name"))?;
                    if engine.inner.db.borrow().durable {
                        return Ok(conflicts::read(&engine.inner.name, &name).await?.into());
                    }
                    let kept = engine.inner.kept.borrow();
                    let (_, bytes) = kept
                        .iter()
                        .find(|(n, _)| *n == name)
                        .ok_or_else(|| js::error(&format!("no conflict copy {name:?}")))?;
                    Ok(Uint8Array::from(bytes.as_slice()).into())
                }),
                "conflictDelete" => Box::pin(async move {
                    let name = js::get_str(&req, "name")
                        .ok_or_else(|| js::error("conflictDelete needs a name"))?;
                    if engine.inner.db.borrow().durable {
                        conflicts::delete(&engine.inner.name, &name).await?;
                    } else {
                        engine.inner.kept.borrow_mut().retain(|(n, _)| *n != name);
                    }
                    Ok(js::object(&[]))
                }),
                _ => return None,
            };
        Some(Ok(future_to_promise(future).into()))
    }

    /// Waits for a running sync call to finish, then removes the session.
    async fn stop_sync(&self) {
        while self.inner.busy.get() {
            js::sleep_ms(50).await;
        }
        *self.inner.sync.borrow_mut() = None;
        *self.inner.settings.borrow_mut() = None;
        self.inner.db.borrow_mut().read_only = false;
    }

    /// Runs one sync call, waiting for any other to finish first.
    async fn run(&self, action: Action) -> Result<(), JsValue> {
        let mut sync = loop {
            if !self.inner.busy.get() {
                match self.inner.sync.borrow_mut().take() {
                    Some(s) => break s,
                    None => return Err(js::error("sync is not set up on this device")),
                }
            }
            js::sleep_ms(50).await;
        };
        self.inner.busy.set(true);
        let mut local = EngineLocal(self.clone());
        let result = match action {
            Action::Connect => sync.connect(&mut local).await,
            Action::Round => sync.sync(&mut local).await,
            Action::Tick => sync.tick(&mut local).await,
            Action::TakeOver => sync.take_over(&mut local).await,
            Action::Disconnect => sync.disconnect(&mut local),
        };
        *self.inner.status.borrow_mut() = Some(sync.status(&self.inner.db.borrow().capture));
        if action != Action::Disconnect && self.inner.settings.borrow().is_some() {
            *self.inner.sync.borrow_mut() = Some(sync);
        }
        self.inner.busy.set(false);
        result.map_err(|e| err(&e))
    }

    fn status_js(&self) -> JsValue {
        let db = self.inner.db.borrow();
        let mut value = match self.inner.status.borrow().as_ref() {
            Some(s) => serde_json::to_value(s).expect("status serializes"),
            None => serde_json::json!({ "role": "none", "connected": false }),
        };
        let settings = self.inner.settings.borrow();
        let obj = value.as_object_mut().expect("an object");
        obj.insert("configured".into(), settings.is_some().into());
        obj.insert("readOnly".into(), db.read_only.into());
        obj.insert("position".into(), db.capture.position().txid.0.into());
        obj.insert(
            "lastCommit".into(),
            db.store.volume().last_commit().0.into(),
        );
        if let Some(s) = settings.as_ref() {
            obj.insert(
                "settings".into(),
                serde_json::json!({
                    "endpoint": s.endpoint, "bucket": s.bucket, "prefix": s.prefix,
                    "region": s.region, "label": s.label, "accessKeyId": s.access_key_id,
                }),
            );
        }
        js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
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
