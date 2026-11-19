//! Opening the database in the leading tab and answering requests.
//!
//! The leader runs the engine (`moul-core`) over the three stores. Every
//! tab's requests reach it as plain objects with an `op`:
//!
//! | `op`              | Fields                      | Answer                                 |
//! | ----------------- | --------------------------- | -------------------------------------- |
//! | `api`             | see [`crate::api`]          | `{ status, headers, json \| bytes }`   |
//! | `subscribe`       | `collection`, `record`?, `events`?, `headers`?, `owner`?, `visible`? | `{ ok, id, leader }` or `{ ok: false, status, json }` |
//! | `unsubscribe`     | `id`                        | `{ ok }`                               |
//! | `export`          |                             | `Uint8Array` of `main.db`              |
//! | `import`          | `bytes`                     | `{ lastCommit }`                       |
//! | `status`          |                             | `{ durable, note, lastCommit, readOnly, role, device }` |
//! | `exec`            | `sql`, `params`?            | `{ changes, lastCommit }`              |
//! | `query`           | `sql`, `params`?            | `{ columns, rows }`                    |
//! | `syncConfigure`   | `endpoint`, `bucket`, …, or `hub`, `database`, `token` | sync status, after connecting |
//! | `syncStatus`      |                             | sync status                            |
//! | `syncNow`         |                             | sync status, after one round           |
//! | `syncJoinTicket`  |                             | `{ hub, database, ticket, expiresAt }` |
//! | `takeOver`        |                             | sync status, after taking over writing |
//! | `syncDisconnect`  | `forget`?                   | sync status                            |
//! | `conflicts`       |                             | names of the conflict copies kept      |
//! | `conflictExport`  | `name`                      | `Uint8Array` of the copy               |
//! | `conflictDelete`  | `name`                      | `{}`                                   |
//!
//! A subscribe answer from a worker names the leading tab that made the
//! subscription in `leader`, the id its `{ type: "leader", tab }` messages
//! carry: a subscription lives in that tab's engine and ends with it, so a
//! page keeps only the answers of the tab that leads now. The worker adds
//! it; [`MoulEngine`](crate::MoulEngine), which has no tabs, leaves it out.
//!
//! `exec` and `query` run the owner's raw SQL on the engine's connection.
//! Writes made that way bypass the change log and cannot be replayed as
//! requests; they stay an owner's tool on a personal database. `query`
//! runs only statements that read: SQLite's authorizer refuses anything
//! else, including setting a pragma, `ATTACH`, `DETACH` and transactions.
//! Those go through `exec`, which announces what it changed.
//!
//! After every request that commits, the leader pushes a notice to every
//! tab: `{ type: "changes", commit, changes, all }`, where `changes` are the
//! new rows of the change log (`seq`, `op`, `collection` and `record` ids,
//! `fields`, `effects`) and `all` says the data changed in a way the log
//! does not describe (raw SQL, an import, pages from the writer). Live
//! queries refresh from it. Subscription events go to the tab that
//! subscribed: `{ type: "event", id, event }`, and `{ type: "closed", id,
//! reason }` when the engine ends a subscription. When the data changes in
//! a way the log does not describe event by event (the device caught up
//! from a snapshot, or a database was imported), every subscription ends
//! that way: its subscriber cannot know what happened in between and loads
//! the data again.
//!
//! `syncConfigure` takes `endpoint`, `bucket`, `accessKeyId` and
//! `secretAccessKey`, and optionally `region`, `prefix`, `pathStyle`,
//! `label` and `followOnly`, to sync with a bucket. To sync through a Moul
//! Hub it takes `hub` (the hub's address), `database` and `token` (the
//! device's token from signing in or joining), and optionally `label` and
//! `followOnly`. The settings, keys and tokens included, stay in this
//! device's own store and never reach the replica; a status shows them
//! without the secret key or the token. The sync requests answer with a
//! promise.
//!
//! While the device syncs through a hub, the leading tab's worker holds the
//! hub's event stream (`GET /api/hub/db/<name>/events`) open and runs a
//! round as soon as another device writes head; it polls only every 30
//! seconds besides, and leaves compaction to the hub. `syncJoinTicket`
//! asks the hub, with the device's own token, for a one-time ticket that
//! lets one more device join the database: a join link carries the ticket,
//! never the token.
//!
//! `syncDisconnect` stops syncing but remembers the replica: the device
//! keeps its level-0 files, so connecting again pushes what it wrote
//! meanwhile, or keeps it as a conflict copy. With `forget: true` it also
//! forgets the replica: the files go, and connecting again later joins as a
//! new device would.
//!
//! While the device follows another writer, the shared store is read-only:
//! the engine answers writes with 409, `exec` runs with `PRAGMA query_only`
//! and `import` is refused.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use js_sys::{Array, Uint8Array};
use moul_core::{Config, Engine as Core, Role as CoreRole, StoreBlobs, SubscriptionId};
use moul_store::{FILES, Store};
use moul_sync::{
    BoxFuture, ConflictCopy, DeviceState, HubConfig, HubReplica, Local, Role, S3Config, S3Replica,
    Status, Sync, store_apply, store_get, store_has_data, store_load_state, store_save_state,
    store_set,
};
use moul_vfs::{
    ApplyMode, Capture, CommitSeq, LOG_HEADER_LEN, LOG_NAME, MemoryStorage, OpfsStorage, PACK_NAME,
    Position, STATE_NAME, Volume,
};
use rusqlite::types::{Value, ValueRef};
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::future_to_promise;

use crate::webhost::WebHost;
use crate::{api, conflicts, js};

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
    /// Whether the device follows another writer.
    pub follower: bool,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("db", &self.db)
            .field("durable", &self.durable)
            .field("note", &self.note)
            .field("follower", &self.follower)
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
/// write to its shared store, so it opens as a follower, and so does one
/// whose take-over has not fenced yet; any other marks a plain imported
/// file as a shared store, as Phase 1 did. Returns whether the device
/// follows.
fn open_stores(volume: Volume) -> Result<(Store, bool), JsValue> {
    let store = Store::open_follower(volume).map_err(|e| err(&e))?;
    let follower = store_load_state(&store)
        .ok()
        .flatten()
        .is_some_and(|s| s.role == Role::Follower || s.unfenced);
    if follower {
        return Ok((store, true));
    }
    let volume = store.close().map_err(|e| err(&e))?;
    Ok((Store::open(volume).map_err(|e| err(&e))?, false))
}

/// Opens a database in memory: for a private window, a browser without
/// OPFS, or a headless engine in tests.
pub fn open_memory(db: &str, note: Option<String>) -> Result<Opened, JsValue> {
    let capture = Capture::open(MemoryStorage::new(), "main.db").map_err(|e| err(&e))?;
    let volume =
        Volume::open_observed(MemoryStorage::new(), capture.clone()).map_err(|e| err(&e))?;
    let (store, follower) = open_stores(volume)?;
    Ok(Opened {
        db: db.to_owned(),
        store,
        capture,
        durable: false,
        note,
        follower,
    })
}

/// Opens the database `db`: on OPFS when the browser offers it, in memory
/// otherwise. Call it only while holding the leader lock.
pub async fn open(db: &str) -> Result<Opened, JsValue> {
    let storage = js::get(&js::navigator(), "storage");
    let has_opfs = !storage.is_undefined() && !js::get(&storage, "getDirectory").is_undefined();
    if !has_opfs {
        return open_memory(
            db,
            Some("this browser has no origin private file system".into()),
        );
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
            let (store, follower) = open_stores(volume)?;
            Ok(Opened {
                db: db.to_owned(),
                store,
                capture,
                durable: true,
                note: None,
                follower,
            })
        }
        Err(e) if opfs_unsupported(e.message()) => open_memory(
            db,
            Some(format!(
                "the origin private file system is not available here ({})",
                e.message()
            )),
        ),
        Err(e) => Err(err(&e)),
    }
}

/// What OPFS holds for a database, as [`stored`] finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stored {
    /// No database by that name, or an empty one.
    Nothing,
    /// A database with something in it, or one that could not be looked at.
    Data,
    /// The browser refused OPFS itself: `getDirectory()` failed with
    /// `UnknownError`, as in WebKit's private windows. Nothing there can be
    /// read, or overwritten. The error's message.
    Refused(String),
}

/// What OPFS holds for the database `db`, without opening it: data is a
/// shared store with any bytes, or a commit in its commit log, where a
/// commit lands before it reaches the shared store.
///
/// A failure other than "not found" counts as data: a database the browser
/// will not open must never be hidden behind an empty one in memory. Except
/// when the browser refuses OPFS itself for no stated reason (`UnknownError`
/// from `getDirectory()`, as WebKit's private windows answer): then nothing
/// stored there can be read or overwritten, whatever else the origin keeps
/// (a service worker's cache, IndexedDB), and running in memory hides
/// nothing.
pub async fn stored(db: &str) -> Stored {
    let storage = js::get(&js::navigator(), "storage");
    let root = match js::call_async(&storage, "getDirectory", &[]).await {
        Ok(root) => root,
        Err(e) if js::get_str(&e, "name").as_deref() == Some("UnknownError") => {
            return Stored::Refused(js::message(&e));
        }
        Err(_) => return Stored::Data,
    };
    let probe = async {
        let mut dir = root;
        for part in opfs_path(db).split('/') {
            dir = js::call_async(&dir, "getDirectoryHandle", &[part.into()]).await?;
        }
        let shared = moul_store::StoreKind::Shared.file_name();
        for (name, empty) in [(shared, 0), (LOG_NAME, LOG_HEADER_LEN)] {
            let size = async {
                let handle = js::call_async(&dir, "getFileHandle", &[name.into()]).await?;
                let file = js::call_async(&handle, "getFile", &[]).await?;
                Ok::<_, JsValue>(js::get(&file, "size").as_f64().unwrap_or(0.0))
            };
            match size.await {
                Ok(size) if size > empty as f64 => return Ok(true),
                Ok(_) => {}
                Err(e) if not_found(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok::<_, JsValue>(false)
    };
    match probe.await {
        Ok(true) => Stored::Data,
        Ok(false) => Stored::Nothing,
        Err(e) if not_found(&e) => Stored::Nothing,
        Err(_) => Stored::Data,
    }
}

/// Whether OPFS holds a database `db` with anything in it; see [`stored`].
pub async fn holds_data(db: &str) -> bool {
    stored(db).await == Stored::Data
}

/// Whether an OPFS error says the entry is not there.
fn not_found(e: &JsValue) -> bool {
    matches!(
        js::get_str(e, "name").as_deref(),
        Some("NotFoundError" | "TypeMismatchError")
    )
}

/// Settings for syncing with an S3-compatible bucket.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BucketSettings {
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
    /// Never become the writer, not even of an empty bucket. A device that
    /// joins from a link uses this, so it cannot upload its own data to a
    /// bucket the link chose.
    #[serde(default)]
    follow_only: bool,
}

/// Settings for syncing through a Moul Hub.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HubSettings {
    /// The hub's address, such as `https://sync.moul.dev`.
    hub: String,
    database: String,
    /// The device's token. Kept in the device store like a bucket's secret
    /// key, and never in a status.
    token: String,
    #[serde(default)]
    label: String,
    /// As for a bucket: a device that joins from a link follows only.
    #[serde(default)]
    follow_only: bool,
}

impl std::fmt::Debug for HubSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubSettings")
            .field("hub", &self.hub)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

/// Sync settings, as `syncConfigure` takes them and the device store keeps
/// them: a bucket, or a database on a hub. Settings stored before hubs
/// existed read as a bucket's.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum SyncSettings {
    Hub(HubSettings),
    Bucket(BucketSettings),
}

/// How often a device that syncs through a hub polls, as a fallback for the
/// hub's event stream.
const HUB_POLL_MS: i64 = 30_000;

impl SyncSettings {
    fn from_js(req: &JsValue) -> Result<Self, JsValue> {
        let text = |k: &str| js::get_str(req, k).unwrap_or_default().trim().to_owned();
        let label = text("label");
        let follow_only = js::get(req, "followOnly").as_bool() == Some(true);
        if !js::get(req, "hub").is_undefined() {
            let s = HubSettings {
                hub: text("hub").trim_end_matches('/').to_owned(),
                database: text("database"),
                token: text("token"),
                label,
                follow_only,
            };
            for (name, v) in [
                ("hub", &s.hub),
                ("database", &s.database),
                ("token", &s.token),
            ] {
                if v.is_empty() {
                    return Err(js::error(&format!("syncConfigure needs {name}")));
                }
            }
            return Ok(SyncSettings::Hub(s));
        }
        let s = BucketSettings {
            endpoint: text("endpoint").trim_end_matches('/').to_owned(),
            bucket: text("bucket"),
            prefix: text("prefix").trim_matches('/').to_owned(),
            region: text("region"),
            access_key_id: text("accessKeyId"),
            secret_access_key: text("secretAccessKey"),
            path_style: js::get(req, "pathStyle").as_bool(),
            label,
            follow_only,
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
        Ok(SyncSettings::Bucket(s))
    }

    fn label(&self) -> &str {
        match self {
            SyncSettings::Hub(h) => &h.label,
            SyncSettings::Bucket(b) => &b.label,
        }
    }

    fn follow_only(&self) -> bool {
        match self {
            SyncSettings::Hub(h) => h.follow_only,
            SyncSettings::Bucket(b) => b.follow_only,
        }
    }

    fn bucket_replica(b: &BucketSettings) -> Result<S3Replica, JsValue> {
        let region = if !b.region.is_empty() {
            b.region.clone()
        } else if b.endpoint.contains("r2.cloudflarestorage.com") {
            "auto".into()
        } else {
            "us-east-1".into()
        };
        S3Replica::with_default_client(S3Config {
            endpoint: b.endpoint.clone(),
            region,
            bucket: b.bucket.clone(),
            prefix: b.prefix.clone(),
            access_key_id: b.access_key_id.clone(),
            secret_access_key: b.secret_access_key.clone(),
            session_token: None,
            path_style: b.path_style.unwrap_or(true),
        })
        .map_err(|e| err(&e))
    }

    fn hub_replica(h: &HubSettings) -> Result<HubReplica, JsValue> {
        HubReplica::with_default_client(HubConfig {
            url: h.hub.clone(),
            database: h.database.clone(),
            token: h.token.clone(),
        })
        .map_err(|e| err(&e))
    }

    fn sync(&self) -> Result<Sync, JsValue> {
        let options = moul_sync::Options {
            label: self.label().to_owned(),
            follow_only: self.follow_only(),
            ..moul_sync::Options::default()
        };
        Ok(match self {
            SyncSettings::Bucket(b) => Sync::new(Self::bucket_replica(b)?, options),
            // The hub's event stream says when to pull; polling is the
            // fallback.
            SyncSettings::Hub(h) => Sync::new(
                Self::hub_replica(h)?,
                moul_sync::Options {
                    poll_every: HUB_POLL_MS,
                    ..options
                },
            ),
        })
    }

    /// The settings a status shows: never a secret key or a token.
    fn public(&self) -> serde_json::Value {
        match self {
            SyncSettings::Bucket(s) => serde_json::json!({
                "endpoint": s.endpoint, "bucket": s.bucket, "prefix": s.prefix,
                "region": s.region, "label": s.label, "accessKeyId": s.access_key_id,
                // The style in use: path style unless the settings said otherwise.
                "pathStyle": s.path_style.unwrap_or(true), "followOnly": s.follow_only,
            }),
            SyncSettings::Hub(h) => serde_json::json!({
                "hub": h.hub, "database": h.database, "label": h.label,
                "followOnly": h.follow_only,
            }),
        }
    }
}

/// Where a hub's event stream is, for the worker that holds it open while
/// the device syncs through the hub.
#[derive(Clone, PartialEq, Eq)]
pub struct HubEvents {
    /// `GET /api/hub/db/<name>/events`.
    pub url: String,
    /// The device's token, sent as `Authorization: Bearer <token>`.
    pub token: String,
}

impl std::fmt::Debug for HubEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubEvents")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// Carries a message from the leader to tabs: to one tab by id, or to all
/// with `None`. The worker posts what reaches its own tab to its page.
pub type Push = Rc<dyn Fn(Option<&str>, JsValue)>;

struct Db {
    core: Core,
    capture: Capture,
    durable: bool,
    note: Option<String>,
    /// The device follows another writer: no writes to the shared store.
    read_only: bool,
    first_write: Option<Box<dyn FnOnce()>>,
    /// Which tab made each subscription.
    subs: HashMap<SubscriptionId, String>,
    /// The change-log position last announced to the tabs.
    announced: i64,
}

struct Inner {
    name: String,
    db: RefCell<Db>,
    push: Push,
    /// The sync session. Taken out while a sync call runs, so requests in
    /// between can still use the database.
    sync: RefCell<Option<Sync>>,
    settings: RefCell<Option<SyncSettings>>,
    busy: Cell<bool>,
    status: RefCell<Option<Status>>,
    /// Conflict copies, when there is no OPFS to keep them in.
    kept: RefCell<Vec<(String, Vec<u8>)>>,
    /// The device has joined a replica. Its level-0 files are then kept
    /// even while sync is off: connecting again pushes what it wrote
    /// meanwhile, or keeps it as a conflict copy, and both read them.
    joined: Cell<bool>,
    /// Why the device could not become the writer, when it could not.
    role_error: RefCell<Option<String>>,
    /// The worker is closing the engine: no new sync rounds.
    closing: Cell<bool>,
    /// Sync replaced the database as a whole during the current sync call
    /// (joining a replica, resyncing after losing the writer role, or a
    /// snapshot). The change log is then not the one subscribers followed.
    replaced: Cell<bool>,
    /// A hub said another device wrote head while a sync call ran: the next
    /// tick runs a full round.
    head_moved: Cell<bool>,
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
        // A snapshot replaces the database whatever the mode. Set before
        // applying: a replace that fails half way changed the data too.
        let snapshot = moul_ltx::Header::from_bytes(ltx).is_ok_and(|h| h.is_snapshot());
        if mode == ApplyMode::Replace || snapshot {
            self.0.inner.replaced.set(true);
        }
        let mut db = self.0.inner.db.borrow_mut();
        let capture = db.capture.clone();
        store_apply(db.core.store_mut(), &capture, ltx, mode)
    }

    fn fence(&mut self) -> moul_sync::Result<moul_ltx::Txid> {
        let db = self.0.inner.db.borrow();
        Ok(db.capture.fence(db.core.store().volume())?)
    }

    fn rebase(&mut self) -> moul_sync::Result<Option<moul_ltx::Txid>> {
        let db = self.0.inner.db.borrow();
        Ok(db.capture.rebase(db.core.store().volume())?)
    }

    fn export(&mut self) -> moul_sync::Result<Vec<u8>> {
        Ok(self.0.inner.db.borrow().core.store().export()?)
    }

    fn has_data(&mut self) -> moul_sync::Result<bool> {
        store_has_data(self.0.inner.db.borrow().core.store())
    }

    fn load_state(&mut self) -> moul_sync::Result<Option<DeviceState>> {
        store_load_state(self.0.inner.db.borrow().core.store())
    }

    fn save_state(&mut self, state: &DeviceState) -> moul_sync::Result<()> {
        store_save_state(self.0.inner.db.borrow().core.store(), state)?;
        self.0.inner.joined.set(state.replica.is_some());
        Ok(())
    }

    fn save_conflict(&mut self, copy: ConflictCopy) -> BoxFuture<'_, moul_sync::Result<String>> {
        Box::pin(async move {
            let inner = &self.0.inner;
            if inner.db.borrow().durable {
                // The status lists the copy by the name the conflict ops take.
                conflicts::save(&inner.name, &copy.name, &copy.bytes)
                    .await
                    .map_err(sync_err)?;
                Ok(copy.name)
            } else {
                inner
                    .kept
                    .borrow_mut()
                    .push((copy.name.clone(), copy.bytes));
                Ok(copy.name)
            }
        })
    }

    fn role_changed(&mut self, role: Role) {
        let role = if role == Role::Follower {
            CoreRole::Follower
        } else {
            CoreRole::Writer
        };
        self.0.set_core_role(role);
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

    /// Whether anything still holds the engine, and with it the files.
    pub fn is_open(&self) -> bool {
        self.0.strong_count() > 0
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("db", &self.name)
            .finish_non_exhaustive()
    }
}

/// The engine's settings in a browser. Mail links point at the page's
/// own address.
fn config() -> Config {
    let origin = js::get_str(&js::get(&js::global(), "location"), "origin")
        .filter(|o| o.starts_with("http"))
        .unwrap_or_else(|| "http://localhost".into());
    Config {
        public_url: origin,
        ..Config::default()
    }
}

/// The most change-log rows one notice lists; past it, the notice says
/// `all`.
const MAX_NOTICE: usize = 10_000;

/// Why subscriptions end when the device caught up from a snapshot.
const CAUGHT_UP: &str = "this device caught up from a snapshot, so the events in between are \
                         unknown; load the data again";

/// Why subscriptions end when a database was imported.
const IMPORTED: &str = "the database was replaced by an import, so the events in between are \
                        unknown; load the data again";

/// A row of the change log as a notice carries it.
fn change_to_js(c: &moul_core::Change) -> JsValue {
    api::from_json(&serde_json::json!({
        "seq": c.seq,
        "request": c.request,
        "op": c.op,
        "collection": c.collection,
        "record": c.record,
        "fields": c.fields,
        "status": c.status,
        "effects": c.effects,
    }))
}

impl Engine {
    /// A handle that does not keep the engine open.
    pub fn downgrade(&self) -> WeakEngine {
        WeakEngine(Rc::downgrade(&self.inner))
    }

    /// Serves `opened`. `first_write` runs once, after the first request that
    /// commits; the worker uses it to ask the page for persistent storage.
    /// `push` carries notices and subscription events to the tabs. Sync
    /// starts if the device was set up for it before.
    pub fn new(
        opened: Opened,
        first_write: Option<Box<dyn FnOnce()>>,
        push: Push,
    ) -> Result<Engine, JsValue> {
        Engine::with_core(opened, first_write, push, |slot| {
            if slot.follower() {
                Core::open_follower(slot.take(), WebHost, StoreBlobs, config())
            } else {
                Core::open(slot.take(), WebHost, StoreBlobs, config())
            }
        })
    }

    /// Serves `opened` with an engine `open` makes from its stores, for a
    /// host with its own clock (a headless test engine).
    pub fn with_core(
        opened: Opened,
        first_write: Option<Box<dyn FnOnce()>>,
        push: Push,
        open: impl FnOnce(&mut OpenSlot) -> Result<Core, moul_core::OpenError>,
    ) -> Result<Engine, JsValue> {
        let settings = store_get(&opened.store, "config")
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<SyncSettings>(&json).ok());
        // A state that cannot be read counts as joined: dropping level-0
        // files a replica may still need cannot be undone.
        let joined = match store_load_state(&opened.store) {
            Ok(state) => state.is_some_and(|s| s.replica.is_some()),
            Err(_) => true,
        };
        let mut slot = OpenSlot {
            store: Some(opened.store),
            follower: opened.follower,
        };
        let core = open(&mut slot).map_err(|e| err(&e))?;
        let announced = core.seen();
        let engine = Engine {
            inner: Rc::new(Inner {
                name: opened.db,
                db: RefCell::new(Db {
                    core,
                    capture: opened.capture,
                    durable: opened.durable,
                    note: opened.note,
                    read_only: opened.follower,
                    first_write,
                    subs: HashMap::new(),
                    announced,
                }),
                push,
                sync: RefCell::new(None),
                settings: RefCell::new(None),
                busy: Cell::new(false),
                status: RefCell::new(None),
                kept: RefCell::new(Vec::new()),
                joined: Cell::new(joined),
                role_error: RefCell::new(None),
                closing: Cell::new(false),
                replaced: Cell::new(false),
                head_moved: Cell::new(false),
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
        Ok(engine)
    }

    /// Whether sync is set up.
    pub fn syncing(&self) -> bool {
        self.inner.settings.borrow().is_some()
    }

    /// Syncs through `sync` as `syncConfigure` does through a bucket, and
    /// answers with the sync status. Nothing is kept in the device store:
    /// for tests, with a [`moul_sync::MemoryReplica`].
    #[doc(hidden)]
    pub async fn connect_with(&self, sync: Sync) -> Result<JsValue, JsValue> {
        self.stop_sync().await;
        let settings = SyncSettings::Bucket(BucketSettings {
            endpoint: sync.replica().describe(),
            bucket: String::new(),
            prefix: String::new(),
            region: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            path_style: None,
            label: String::new(),
            follow_only: false,
        });
        *self.inner.settings.borrow_mut() = Some(settings);
        *self.inner.sync.borrow_mut() = Some(sync);
        self.run(Action::Connect).await?;
        Ok(self.status_js())
    }

    /// Runs one round of background sync if one is due and none is running.
    /// The worker calls this about once a second.
    pub async fn tick(&self) {
        if self.inner.sync.borrow().is_some() && !self.inner.busy.get() && !self.closing() {
            let action = if self.inner.head_moved.replace(false) {
                Action::Round
            } else {
                Action::Tick
            };
            let _ = self.run(action).await;
        }
    }

    /// Where the hub's event stream is, while the device syncs through a
    /// hub; `None` otherwise.
    pub fn hub_events(&self) -> Option<HubEvents> {
        match self.inner.settings.borrow().as_ref()? {
            SyncSettings::Hub(h) => Some(HubEvents {
                url: SyncSettings::hub_replica(h).ok()?.events_url(),
                token: h.token.clone(),
            }),
            SyncSettings::Bucket(_) => None,
        }
    }

    /// The hub's event stream says a device wrote head, `data` being the
    /// new `moul/head.json`: a follower pulls at once, and a writer checks
    /// it still writes. Nothing to do for this device's own writes. If a
    /// sync call is running, the next tick runs the round.
    pub async fn on_head(&self, data: &str) {
        if let Ok(head) = serde_json::from_str::<moul_sync::layout::Head>(data)
            && self
                .inner
                .status
                .borrow()
                .as_ref()
                .is_some_and(|s| !s.device.is_empty() && s.device == head.writer)
        {
            return;
        }
        if self.inner.sync.borrow().is_none() || self.closing() {
            return;
        }
        if self.inner.busy.get() {
            self.inner.head_moved.set(true);
            return;
        }
        let _ = self.run(Action::Round).await;
    }

    /// Starts no more sync rounds: the worker is closing the engine. A
    /// round already running finishes, and holds the engine until then.
    pub fn shut_down(&self) {
        self.inner.closing.set(true);
    }

    /// Whether [`shut_down`](Self::shut_down) was called.
    pub fn closing(&self) -> bool {
        self.inner.closing.get()
    }

    /// Makes the engine a writer or a follower. A device that cannot become
    /// the writer (migrating its shared store failed) stays read-only, and
    /// the sync status and the `status` request say why.
    fn set_core_role(&self, role: CoreRole) {
        let mut db = self.inner.db.borrow_mut();
        match db.core.set_role(role) {
            Ok(()) => {
                db.read_only = role == CoreRole::Follower;
                *self.inner.role_error.borrow_mut() = None;
            }
            Err(e) => {
                db.read_only = true;
                *self.inner.role_error.borrow_mut() = Some(format!(
                    "this device stays read-only: it could not become the writer ({e})"
                ));
            }
        }
    }

    /// Ends the subscriptions of tabs that are gone. `live` holds the ids
    /// of the tabs still open.
    pub fn prune(&self, live: &HashSet<String>) {
        let mut db = self.inner.db.borrow_mut();
        let gone: Vec<SubscriptionId> = db
            .subs
            .iter()
            .filter(|(_, tab)| !live.contains(*tab))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            db.subs.remove(&id);
            db.core.unsubscribe(id);
        }
    }

    /// The subscriptions open now, for tests.
    pub fn subscription_count(&self) -> usize {
        self.inner.db.borrow().subs.len()
    }

    /// Answers one request from tab `from`. Sync requests answer with a
    /// promise.
    pub fn handle(&self, req: JsValue, from: &str) -> Result<JsValue, JsValue> {
        let op = js::get_str(&req, "op").ok_or_else(|| js::error("request has no op"))?;
        if let Some(answer) = self.handle_sync(&op, &req) {
            return answer;
        }
        if self.inner.replaced.get() {
            // Sync replaced the database while its call waits on the
            // network: end the subscriptions that followed the old data
            // before answering, so that this request starts on the new data.
            // A subscription it makes must not end with the old ones.
            self.announce(self.last_commit(), false, None);
        }
        let before = self.last_commit();
        let answer = self.answer(&op, &req, from);
        // Raw SQL and imports change data the change log does not describe.
        // An import replaces all of it once the volume took the file, even
        // if it failed after that (reopening the store, or migrating it).
        let imported = op == "import" && self.last_commit() != before;
        self.announce(
            before,
            matches!(op.as_str(), "exec" | "import"),
            imported.then_some(IMPORTED),
        );
        answer
    }

    fn last_commit(&self) -> CommitSeq {
        self.inner.db.borrow().core.store().volume().last_commit()
    }

    /// After a request or a sync round: runs the first-write hook, drops
    /// level-0 files nothing reads, and pushes the change notice and
    /// subscription events. `all` marks changes the log does not describe;
    /// `replaced` says why the data was replaced as a whole, if it was.
    ///
    /// Sync may have replaced the database while a sync call waits on the
    /// network, before the call ends: whichever announcement comes first
    /// after that, the call's own or one a request makes before it is
    /// answered, ends every subscription.
    fn announce(&self, before: CommitSeq, mut all: bool, replaced: Option<&str>) {
        let replaced = match replaced {
            Some(reason) => {
                self.inner.replaced.set(false);
                Some(reason)
            }
            None => self.inner.replaced.replace(false).then_some(CAUGHT_UP),
        };
        let mut db = self.inner.db.borrow_mut();
        let changed = db.core.store().volume().last_commit() != before || replaced.is_some();
        let mut changes = Vec::new();
        if changed {
            if let Some(f) = db.first_write.take() {
                f();
            }
            // A device that never joined a replica has nothing to push: no
            // one reads its level-0 files, so drop them. One that joined
            // keeps them while disconnected, for the next connect.
            if self.inner.settings.borrow().is_none() && !self.inner.joined.get() {
                let at = db.capture.position().txid;
                let _ = db.capture.release(at);
            }
            if let Some(reason) = replaced {
                // The log is not the one subscribers followed, wherever it
                // ends: end them before reading it, and start after it.
                db.core.end_subscriptions(reason);
                let newest = newest_seq(db.core.store());
                db.core.set_seen(newest);
                db.announced = newest;
                all = true;
            }
            // A follower learns of the writer's requests from the log; this
            // queues their events for subscriptions. On the writer, its own
            // requests were announced as they ran, and this finds nothing.
            db.core.catch_up();
            let caught_up_to = db.core.seen();
            changes = db.core.changes(db.announced, MAX_NOTICE);
            if let Some(last) = changes.last() {
                db.announced = last.seq;
            }
            if all || changes.len() == MAX_NOTICE {
                // The log may have been replaced by a snapshot, or be too long
                // to list: refresh everything and continue from its end.
                all = true;
                let newest = newest_seq(db.core.store());
                // Rows the engine never turned into events, or a log that
                // is not the one it read: subscribers would miss events
                // and keep records that are gone.
                if replaced.is_none() && newest != caught_up_to {
                    db.core.end_subscriptions(CAUGHT_UP);
                }
                db.announced = newest;
                db.core.set_seen(newest);
            }
        }
        let deliveries = db.core.take_deliveries();
        let closed = db.core.take_closed();
        let mut routes = Vec::with_capacity(deliveries.len() + closed.len());
        for d in deliveries {
            if let Some(tab) = db.subs.get(&d.subscription) {
                let event = serde_json::to_value(&d.event).expect("events serialize");
                routes.push((
                    tab.clone(),
                    js::object(&[
                        ("type", "event".into()),
                        ("id", (d.subscription as f64).into()),
                        ("event", api::from_json(&event)),
                    ]),
                ));
            }
        }
        for (id, reason) in closed {
            if let Some(tab) = db.subs.remove(&id) {
                routes.push((
                    tab,
                    js::object(&[
                        ("type", "closed".into()),
                        ("id", (id as f64).into()),
                        ("reason", reason.into()),
                    ]),
                ));
            }
        }
        let commit = db.core.store().volume().last_commit().0;
        drop(db);
        if changed {
            let list: Array = changes.iter().map(change_to_js).collect();
            (self.inner.push)(
                None,
                js::object(&[
                    ("type", "changes".into()),
                    ("commit", (commit as f64).into()),
                    ("changes", list.into()),
                    ("all", all.into()),
                ]),
            );
        }
        for (tab, msg) in routes {
            (self.inner.push)(Some(&tab), msg);
        }
    }

    fn answer(&self, op: &str, req: &JsValue, from: &str) -> Result<JsValue, JsValue> {
        let mut db = self.inner.db.borrow_mut();
        let read_only = db.read_only;
        match op {
            "api" => {
                let request = api::to_request(req)?;
                let response = db.core.handle(request);
                Ok(api::response_to_js(&response))
            }
            "subscribe" => {
                let sub = api::to_subscription(req)?;
                match db.core.subscribe(sub) {
                    Ok(id) => {
                        let visible = api::visible(req);
                        if !visible.is_empty() {
                            db.core.note_visible(id, &visible);
                        }
                        db.subs.insert(id, from.to_owned());
                        Ok(js::object(&[
                            ("ok", true.into()),
                            ("id", (id as f64).into()),
                        ]))
                    }
                    Err(r) => {
                        let out = api::response_to_js(&r);
                        let _ = js_sys::Reflect::set(&out, &"ok".into(), &false.into());
                        Ok(out)
                    }
                }
            }
            "unsubscribe" => {
                let id = js::get(req, "id")
                    .as_f64()
                    .ok_or_else(|| js::error("unsubscribe needs an id"))?
                    as SubscriptionId;
                let ok = db.subs.get(&id).is_some_and(|tab| tab == from) && db.core.unsubscribe(id);
                if ok {
                    db.subs.remove(&id);
                }
                Ok(js::object(&[("ok", ok.into())]))
            }
            "exec" => {
                let sql = js::get_str(req, "sql").ok_or_else(|| js::error("exec needs sql"))?;
                let params = params(&js::get(req, "params"))?;
                let store = db.core.store();
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
                db.core.read_only(|store| query(store, &sql, params))
            }
            "export" => {
                let bytes = db.core.store().export().map_err(|e| err(&e))?;
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
                db.core
                    .import_database(&bytes.to_vec())
                    .map_err(|e| err(&e))?;
                let seq = db.core.store().volume().last_commit();
                Ok(js::object(&[("lastCommit", (seq.0 as f64).into())]))
            }
            "status" => Ok(js::object(&[
                ("durable", db.durable.into()),
                (
                    "note",
                    match (
                        db.note.as_deref(),
                        self.inner.role_error.borrow().as_deref(),
                    ) {
                        (Some(a), Some(b)) => format!("{a}; {b}").into(),
                        (a, b) => a.or(b).map_or(JsValue::NULL, JsValue::from),
                    },
                ),
                (
                    "lastCommit",
                    (db.core.store().volume().last_commit().0 as f64).into(),
                ),
                ("readOnly", read_only.into()),
                ("role", if read_only { "follower" } else { "writer" }.into()),
                ("device", db.core.device_id().into()),
                ("syncing", self.syncing().into()),
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
                        store_set(db.core.store(), "config", Some(&json)).map_err(|e| err(&e))?;
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
                "syncJoinTicket" => Box::pin(async move {
                    let settings = engine.inner.settings.borrow().clone();
                    let Some(SyncSettings::Hub(h)) = settings else {
                        return Err(js::error(
                            "join tickets come from a hub, and this device does not sync through one",
                        ));
                    };
                    let ticket = SyncSettings::hub_replica(&h)?
                        .create_join_ticket()
                        .await
                        .map_err(|e| err(&e))?;
                    Ok(js::object(&[
                        ("hub", h.hub.into()),
                        ("database", h.database.into()),
                        ("ticket", ticket.ticket.into()),
                        ("expiresAt", (ticket.expires_at as f64).into()),
                    ]))
                }),
                "takeOver" => Box::pin(async move {
                    engine.run(Action::TakeOver).await?;
                    Ok(engine.status_js())
                }),
                "syncDisconnect" => Box::pin(async move {
                    let forget = js::get(&req, "forget").as_bool() == Some(true);
                    // Whatever fails, the session and the settings go, and
                    // the replica is forgotten if asked; then the first
                    // error is the answer.
                    let ran = if engine.inner.sync.borrow().is_some() || engine.inner.busy.get() {
                        engine.run(Action::Disconnect).await
                    } else {
                        Ok(())
                    };
                    engine.stop_sync().await;
                    let (cleared, forgot) = {
                        let db = engine.inner.db.borrow();
                        let cleared =
                            store_set(db.core.store(), "config", None).map_err(|e| err(&e));
                        let forgot = if forget {
                            engine.forget_replica(&db)
                        } else {
                            // Should the session not have disconnected, the
                            // state still says the device follows: it would
                            // open read-only for good, with no sync to
                            // settle its role.
                            no_role(db.core.store())
                        };
                        (cleared, forgot)
                    };
                    *engine.inner.status.borrow_mut() = None;
                    ran.and(cleared).and(forgot)?;
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

    /// Forgets the replica the device joined, for `syncDisconnect` with
    /// `forget`: the device counts as never having joined one, so the
    /// level-0 files it kept for the next connect go now, and after every
    /// commit from then on. Connecting again later joins as a new device
    /// would, under a new device id: what the device holds then is kept as a
    /// conflict copy. Keeping the id would let a writer that forgot pass for
    /// the writer of record again.
    fn forget_replica(&self, db: &Db) -> Result<(), JsValue> {
        let store = db.core.store();
        if !matches!(store_load_state(store), Ok(None)) {
            store_save_state(store, &DeviceState::default()).map_err(|e| err(&e))?;
        }
        self.inner.joined.set(false);
        let at = db.capture.position().txid;
        db.capture.release(at).map_err(|e| err(&e))?;
        Ok(())
    }

    /// Waits for a running sync call to finish, then removes the session.
    async fn stop_sync(&self) {
        while self.inner.busy.get() {
            js::sleep_ms(50).await;
        }
        *self.inner.sync.borrow_mut() = None;
        *self.inner.settings.borrow_mut() = None;
        let read_only = self.inner.db.borrow().read_only;
        if read_only {
            self.set_core_role(CoreRole::Writer);
        }
    }

    /// Runs one sync call, waiting for any other to finish first.
    async fn run(&self, action: Action) -> Result<(), JsValue> {
        let mut sync = loop {
            if self.closing() {
                return Err(js::error("the database is closing"));
            }
            if !self.inner.busy.get() {
                match self.inner.sync.borrow_mut().take() {
                    Some(s) => break s,
                    None => return Err(js::error("sync is not set up on this device")),
                }
            }
            js::sleep_ms(50).await;
        };
        self.inner.busy.set(true);
        self.inner.replaced.set(false);
        let before = self.last_commit();
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
        // Pages from the writer changed the data underneath the engine; a
        // replaced database ends every subscription, unless a request
        // announced that already.
        self.announce(before, true, None);
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
        if let Some(e) = self.inner.role_error.borrow().as_ref() {
            let text = match obj.get("lastError").and_then(|v| v.as_str()) {
                Some(other) => format!("{e}; {other}"),
                None => e.clone(),
            };
            obj.insert("lastError".into(), text.into());
        }
        obj.insert("configured".into(), settings.is_some().into());
        obj.insert("readOnly".into(), db.read_only.into());
        obj.insert("position".into(), db.capture.position().txid.0.into());
        obj.insert(
            "lastCommit".into(),
            db.core.store().volume().last_commit().0.into(),
        );
        if let Some(s) = settings.as_ref() {
            obj.insert("settings".into(), s.public());
        }
        api::from_json(&value)
    }
}

/// The stores an engine is about to open, for [`Engine::with_core`].
#[derive(Debug)]
pub struct OpenSlot {
    store: Option<Store>,
    follower: bool,
}

impl OpenSlot {
    /// Takes the stores.
    pub fn take(&mut self) -> Store {
        self.store.take().expect("the stores are taken once")
    }

    /// Whether the device follows another writer.
    pub fn follower(&self) -> bool {
        self.follower
    }
}

/// Records in the device state that the device has no role, as
/// disconnecting does, keeping the replica it joined. A state that cannot
/// be read is left as it is.
fn no_role(store: &Store) -> Result<(), JsValue> {
    match store_load_state(store) {
        Ok(Some(mut state)) if state.role != Role::None => {
            state.role = Role::None;
            store_save_state(store, &state).map_err(|e| err(&e))
        }
        _ => Ok(()),
    }
}

/// The newest position in the change log, or 0 before the shared store has
/// one.
fn newest_seq(store: &Store) -> i64 {
    store
        .conn()
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM main._changes", [], |r| {
            r.get(0)
        })
        .unwrap_or(0)
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

/// Runs one statement that reads, under the engine's
/// [`read_only`](Core::read_only) authorizer. One that writes would change
/// data without the change being announced, even on a follower; one that
/// sets a pragma, attaches a database or opens a transaction would change
/// the connection every later request runs on. Both are refused: `exec`
/// runs those.
fn query(store: &Store, sql: &str, params: Vec<Value>) -> Result<JsValue, JsValue> {
    let refused = || {
        js::error(
            "query runs only statements that read; use exec to change data, the schema or a \
             setting, or to run a transaction",
        )
    };
    let fail = |e: rusqlite::Error| {
        if not_authorized(&e) {
            refused()
        } else {
            err(&e)
        }
    };
    let conn = store.conn();
    let mut stmt = conn.prepare(sql).map_err(fail)?;
    if !stmt.readonly() {
        return Err(refused());
    }
    let columns: Array = stmt.column_names().map(JsValue::from).collect();
    let n = stmt.column_count();
    let rows = Array::new();
    let mut it = stmt
        .query(rusqlite::params_from_iter(params))
        .map_err(fail)?;
    while let Some(row) = it.next().map_err(fail)? {
        let out = Array::new();
        for i in 0..n {
            out.push(&to_js(row.get_ref(i).map_err(fail)?));
        }
        rows.push(&out);
    }
    Ok(js::object(&[
        ("columns", columns.into()),
        ("rows", rows.into()),
    ]))
}

/// Whether SQLite's authorizer refused the statement. It says so in the
/// message, with `SQLITE_AUTH` except for a function it refuses.
fn not_authorized(e: &rusqlite::Error) -> bool {
    let code = match e {
        rusqlite::Error::SqliteFailure(f, _) | rusqlite::Error::SqlInputError { error: f, .. } => {
            Some(f.code)
        }
        _ => None,
    };
    code == Some(rusqlite::ErrorCode::AuthorizationForStatementDenied)
        || e.to_string().contains("not authorized")
}
