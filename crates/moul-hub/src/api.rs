//! The hub's HTTP API, as `docs/hub.md` describes it.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::stream;
use moul_server::http::{AppState, ClientIp, error, to_http};
use moul_sync::{Condition, Put, block_on};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::directory::{self, ACCOUNTS, Device};
use crate::store::{Store, valid_key};

/// Uploads one account may have in flight at once.
const UPLOADS_PER_ACCOUNT: usize = 4;

/// Megabytes of upload bodies the hub holds in memory at once.
const MEMORY_MIB: usize = 512;

/// Who may sign up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signup {
    /// Anyone.
    Open,
    /// With an invite code.
    Invite,
    /// Only accounts the admin made.
    Closed,
}

/// The hub's own state, beside the directory's server state.
pub struct Hub {
    /// Where replicas live.
    pub store: Store,
    /// Who may sign up.
    pub signup: Signup,
    /// Databases per account.
    pub max_databases: i64,
    /// Bytes per account.
    pub max_bytes: i64,
    /// Bytes per object.
    pub max_object: usize,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    uploads: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    /// Megabytes of upload bodies held in memory at once, across accounts.
    memory: Arc<tokio::sync::Semaphore>,
    events: Mutex<HashMap<String, broadcast::Sender<String>>>,
    asks: Mutex<HashMap<String, (i64, u32)>>,
}

impl Hub {
    /// A hub over `store`.
    pub fn new(store: Store, signup: Signup, max_databases: i64, max_bytes: i64) -> Hub {
        Hub {
            store,
            signup,
            max_databases,
            max_bytes,
            max_object: 256 << 20,
            locks: Mutex::default(),
            uploads: Mutex::default(),
            memory: Arc::new(tokio::sync::Semaphore::new(MEMORY_MIB)),
            events: Mutex::default(),
            asks: Mutex::default(),
        }
    }

    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_owned())
            .or_default()
            .clone()
    }

    /// Uploads one account may have in flight at once.
    fn uploads_for(&self, account: &str) -> Arc<tokio::sync::Semaphore> {
        self.uploads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(account.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(UPLOADS_PER_ACCOUNT)))
            .clone()
    }

    fn sender(&self, key: &str) -> broadcast::Sender<String> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_owned())
            .or_insert_with(|| broadcast::channel(16).0)
            .clone()
    }

    /// Ends every open event stream, as when the hub stops.
    pub fn end_events(&self) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Counts a request for a code against `key`: at most `max` an hour.
    fn allow_ask(&self, key: &str, max: u32) -> bool {
        let now = moul_vfs::system_clock();
        let mut asks = self.asks.lock().unwrap_or_else(|e| e.into_inner());
        if asks.len() > 50_000 {
            asks.retain(|_, (t, _)| now - *t < 3_600_000);
        }
        let e = asks.entry(key.to_owned()).or_insert((now, 0));
        if now - e.0 >= 3_600_000 {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= max
    }
}

/// The routes, for the directory's middleware to wrap.
pub fn routes(hub: Arc<Hub>) -> Router<AppState> {
    let h = hub;
    Router::new()
        .route("/api/hub/signin", post(signin))
        .route("/api/hub/verify", post(verify))
        .route("/api/hub/join", post(join))
        .route("/api/hub/me", get(me))
        .route("/api/hub/devices", get(list_devices))
        .route(
            "/api/hub/devices/{id}",
            axum::routing::delete(delete_device),
        )
        .route(
            "/api/hub/databases",
            get(list_databases).post(create_database),
        )
        .route(
            "/api/hub/databases/{name}",
            axum::routing::delete(delete_database),
        )
        .route("/api/hub/databases/{name}/joins", post(create_join))
        .route(
            "/api/hub/db/{name}/o/{*key}",
            get(get_object).put(put_object).delete(delete_object),
        )
        .route("/api/hub/db/{name}/list", get(list_objects))
        .route("/api/hub/db/{name}/events", get(events))
        .route(
            "/api/hub/admin/accounts",
            get(admin_accounts).post(admin_create_account),
        )
        .route("/api/hub/admin/invites", post(admin_invite))
        // The directory's settings (mail) and inbox, for the admin; the
        // engine checks the admin key.
        .route(
            "/api/settings",
            get(moul_server::http::engine).patch(moul_server::http::engine),
        )
        .route("/api/settings/reload", post(moul_server::http::engine))
        .route("/api/inbox", get(moul_server::http::engine))
        .layer(axum::Extension(h))
}

type Ext = axum::Extension<Arc<Hub>>;

fn bearer(headers: &HeaderMap) -> Option<String> {
    let h = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = h.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_owned())
}

/// The device a request's token belongs to.
async fn device(state: &AppState, headers: &HeaderMap) -> Result<Device, Response> {
    let Some(token) = bearer(headers) else {
        return Err(error(
            401,
            "Sign in to the hub: send a device token as Authorization: Bearer",
        ));
    };
    state
        .db
        .run(move |db| directory::device_for_token(db, &token))
        .await
        .map_err(|_| error(503, "The hub is closing"))?
        .ok_or_else(|| error(401, "This device's token is not valid; sign in again"))
}

/// The device, which must be an account's (not one database's).
async fn account_device(state: &AppState, headers: &HeaderMap) -> Result<Device, Response> {
    let d = device(state, headers).await?;
    if d.database.is_some() {
        return Err(error(
            403,
            "This device reaches one database only; sign in with your email for the rest",
        ));
    }
    Ok(d)
}

async fn body_json(req: Request) -> Value {
    let bytes = axum::body::to_bytes(req.into_body(), 1 << 16)
        .await
        .unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").trim().to_owned()
}

fn status_json(status: u16, v: Value) -> Response {
    let mut r = axum::Json(v).into_response();
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    r
}

fn plausible_email(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && e.len() <= 254
        && !e.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `POST /api/hub/signin` with `{email, invite?}`: a code goes out by mail
/// if this email may sign in. The answer is the same either way.
async fn signin(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    req: Request,
) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0);
    let b = body_json(req).await;
    let email = text(&b, "email").to_ascii_lowercase();
    if !plausible_email(&email) {
        return error(400, "Give an email address");
    }
    let ip_key = ip.map_or_else(String::new, moul_server::limits::network);
    if !hub.allow_ask(&format!("ip:{ip_key}"), 30) || !hub.allow_ask(&format!("email:{email}"), 6) {
        return error(429, "Too many codes asked for; wait a while");
    }
    let invite = text(&b, "invite");
    let signup = hub.signup;
    let remote = ip.map(|i| i.to_string());
    let _ = state
        .db
        .run(move |db| {
            let exists = directory::account_by_email(db, &email).is_some();
            let may = exists
                || match signup {
                    Signup::Open => true,
                    Signup::Invite => !invite.is_empty() && directory::take_invite(db, &invite),
                    Signup::Closed => false,
                };
            if may {
                let mut r = moul_core::Request::post(
                    &format!("/api/moul/{ACCOUNTS}/otp/request"),
                    json!({"email": email}),
                );
                r.remote = remote;
                db.handle(r);
            }
        })
        .await;
    axum::Json(json!({"sent": true})).into_response()
}

/// `POST /api/hub/verify` with `{email, code, label?}`: an account token.
async fn verify(State(state): State<AppState>, req: Request) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0.to_string());
    let b = body_json(req).await;
    let (email, code, label) = (
        text(&b, "email").to_ascii_lowercase(),
        text(&b, "code"),
        text(&b, "label"),
    );
    if email.is_empty() || code.is_empty() {
        return error(400, "email and code are required");
    }
    let out = state
        .db
        .run(move |db| {
            let mut r = moul_core::Request::post(
                &format!("/api/moul/{ACCOUNTS}/auth-with-otp"),
                json!({"email": email, "code": code}),
            );
            r.remote = ip;
            let answer = db.handle(r);
            if answer.status != 200 {
                return Err(answer);
            }
            let id = answer.json_body()["record"]["id"].as_str().unwrap_or("").to_owned();
            let account = directory::account(db, &id).ok_or_else(|| moul_core::ApiError::internal().into_response())?;
            let (token, device) = directory::create_device(db, &account.id, None, &label)
                .map_err(|_| moul_core::ApiError::internal().into_response())?;
            Ok(json!({"token": token, "device": device.to_json(&device.id), "account": account.to_json()}))
        })
        .await;
    match out {
        Ok(Ok(v)) => axum::Json(v).into_response(),
        Ok(Err(r)) => to_http(r, None),
        Err(e) => e.into_response(),
    }
}

/// `POST /api/hub/join` with `{ticket, label?}`: a token for one database.
async fn join(State(state): State<AppState>, req: Request) -> Response {
    let b = body_json(req).await;
    let (ticket, label) = (text(&b, "ticket"), text(&b, "label"));
    if ticket.is_empty() {
        return error(400, "ticket is required");
    }
    let out = state
        .db
        .run(move |db| {
            let (account, database) = directory::take_join(db, &ticket)?;
            directory::database(db, &account, &database)?;
            let (token, device) =
                directory::create_device(db, &account, Some(&database), &label).ok()?;
            Some(
                json!({"token": token, "device": device.to_json(&device.id), "database": database}),
            )
        })
        .await;
    match out {
        Ok(Some(v)) => axum::Json(v).into_response(),
        Ok(None) => error(
            400,
            "This link was used already or has expired; ask for a new one",
        ),
        Err(e) => e.into_response(),
    }
}

/// `GET /api/hub/me`
async fn me(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    headers: HeaderMap,
) -> Response {
    let d = match device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let (max_db, max_bytes) = (hub.max_databases, hub.max_bytes);
    let out = state
        .db
        .run(move |db| {
            // A device that joined one database learns of that database
            // only, not the account's email or its other databases.
            let (account, (n, bytes)) = match &d.database {
                None => (
                    directory::account(db, &d.account).map(|a| a.to_json()),
                    directory::usage(db, &d.account),
                ),
                Some(name) => (
                    Some(json!({"id": d.account})),
                    (
                        1,
                        directory::database(db, &d.account, name).map_or(0, |x| x.bytes),
                    ),
                ),
            };
            json!({
                "account": account,
                "device": d.to_json(&d.id),
                "limits": {"databases": max_db, "bytes": max_bytes},
                "usage": {"databases": n, "bytes": bytes},
            })
        })
        .await;
    match out {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /api/hub/devices`
async fn list_devices(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let d = match account_device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    match state
        .db
        .run(move |db| {
            directory::devices(db, &d.account)
                .iter()
                .map(|x| x.to_json(&d.id))
                .collect::<Vec<_>>()
        })
        .await
    {
        Ok(items) => axum::Json(json!({"items": items})).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `DELETE /api/hub/devices/:id`: an account's device may revoke any of
/// the account's devices; a database's device, only itself.
async fn delete_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let d = match device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    if d.database.is_some() && d.id != id {
        return error(403, "This device may only sign itself out");
    }
    match state
        .db
        .run(move |db| directory::delete_device(db, &d.account, &id))
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(404, "No such device"),
        Err(e) => e.into_response(),
    }
}

async fn head_of(hub: &Hub, account: &str, name: &str) -> Value {
    hub.store
        .with(account, name, |r| {
            Ok(block_on(r.get(moul_sync::layout::HEAD_KEY))
                .ok()
                .flatten()
                .and_then(|o| serde_json::from_slice::<Value>(&o.bytes).ok())
                .unwrap_or(Value::Null))
        })
        .await
        .unwrap_or(Value::Null)
}

fn database_json(d: &directory::Database, head: Value) -> Value {
    json!({"name": d.name, "createdAt": d.created_at, "bytes": d.bytes, "head": head})
}

/// `GET /api/hub/databases`
async fn list_databases(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    headers: HeaderMap,
) -> Response {
    let d = match device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let (account, only) = (d.account.clone(), d.database.clone());
    let list = state
        .db
        .run(move |db| {
            directory::databases(db, &account)
                .into_iter()
                .filter(|x| only.as_deref().is_none_or(|o| o == x.name))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
    let mut items = Vec::new();
    for x in &list {
        items.push(database_json(x, head_of(&hub, &d.account, &x.name).await));
    }
    axum::Json(json!({"items": items})).into_response()
}

/// `POST /api/hub/databases` with `{name}`.
async fn create_database(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    req: Request,
) -> Response {
    let headers = req.headers().clone();
    let d = match account_device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let name = text(&body_json(req).await, "name");
    if !moul_sync::hub::valid_database_name(&name) {
        return error(
            400,
            "A database name is 1 to 63 of a-z, 0-9 and -, starting with a letter or digit",
        );
    }
    let max = hub.max_databases;
    let out = state
        .db
        .run(move |db| {
            if directory::database(db, &d.account, &name).is_some() {
                return Err(error(
                    409,
                    &format!("You already have a database named {name}"),
                ));
            }
            if directory::usage(db, &d.account).0 >= max {
                return Err(error(
                    403,
                    &format!("An account keeps at most {max} databases"),
                ));
            }
            directory::create_database(db, &d.account, &name)
                .ok()
                .flatten()
                .ok_or_else(|| error(409, &format!("You already have a database named {name}")))
        })
        .await;
    match out {
        Ok(Ok(x)) => status_json(201, database_json(&x, Value::Null)),
        Ok(Err(r)) => r,
        Err(e) => e.into_response(),
    }
}

/// `DELETE /api/hub/databases/:name`: the replica goes, and the devices
/// that reached only it.
async fn delete_database(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let d = match account_device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let (a, n) = (d.account.clone(), name.clone());
    if !matches!(
        state
            .db
            .run(move |db| directory::database(db, &a, &n).is_some())
            .await,
        Ok(true)
    ) {
        return error(404, &format!("No database named {name}"));
    }
    let lock = hub.lock_for(&format!("{}/{name}", d.account));
    let _guard = lock.lock().await;
    if let Err(e) = hub.store.delete_all(&d.account, &name).await {
        return error(502, &format!("The store refused: {e}"));
    }
    let (a, n) = (d.account.clone(), name.clone());
    let _ = state
        .db
        .run(move |db| directory::delete_database(db, &a, &n))
        .await;
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/hub/databases/:name/joins`
async fn create_join(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let d = match device(&state, &headers).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    if !d.reaches(&name) {
        return error(403, "This device does not reach that database");
    }
    let out = state
        .db
        .run(move |db| {
            directory::database(db, &d.account, &name)?;
            directory::create_join(db, &d.account, &name).ok()
        })
        .await;
    match out {
        Ok(Some((ticket, expires))) => {
            status_json(201, json!({"ticket": ticket, "expiresAt": expires}))
        }
        Ok(None) => error(404, "No such database"),
        Err(e) => e.into_response(),
    }
}

/// The device and database a replica call is for, checked.
async fn replica_access(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
) -> Result<Device, Response> {
    let d = device(state, headers).await?;
    if !d.reaches(name) {
        return Err(error(403, "This device does not reach that database"));
    }
    let (a, n) = (d.account.clone(), name.to_owned());
    match state
        .db
        .run(move |db| directory::database(db, &a, &n).is_some())
        .await
    {
        Ok(true) => Ok(d),
        Ok(false) => Err(error(404, &format!("No database named {name}"))),
        Err(e) => Err(e.into_response()),
    }
}

/// An entity tag without `W/` or quotes.
fn bare(etag: &str) -> String {
    etag.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .to_owned()
}

fn etag_header(etag: &str) -> HeaderValue {
    HeaderValue::try_from(format!("\"{}\"", bare(etag))).unwrap_or(HeaderValue::from_static("\"\""))
}

/// `GET /api/hub/db/:name/o/<key>`
async fn get_object(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    Path((name, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let d = match replica_access(&state, &headers, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    if !valid_key(&key) {
        return error(400, "Not a replica key");
    }
    match hub
        .store
        .with(&d.account, &name, move |r| {
            block_on(r.get(&key)).map_err(|e| e.to_string())
        })
        .await
    {
        Ok(Some(o)) => {
            let mut resp = Response::new(Body::from(o.bytes));
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            h.insert(header::ETAG, etag_header(&o.etag));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Ok(None) => error(404, "No such object"),
        Err(e) => error(502, &format!("The store refused: {e}")),
    }
}

fn condition(headers: &HeaderMap) -> Result<Condition, Response> {
    let get = |n: header::HeaderName| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim);
    match (get(header::IF_NONE_MATCH), get(header::IF_MATCH)) {
        (Some("*"), None) => Ok(Condition::IfNoneMatch),
        (None, Some(tag)) if !tag.is_empty() => Ok(Condition::IfMatch(bare(tag))),
        (None, None) => Ok(Condition::None),
        _ => Err(error(400, "Send If-None-Match: * or one If-Match")),
    }
}

/// `PUT /api/hub/db/:name/o/<key>`
async fn put_object(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    Path((name, key)): Path<(String, String)>,
    req: Request,
) -> Response {
    let headers = req.headers().clone();
    let d = match replica_access(&state, &headers, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    if !valid_key(&key) {
        return error(400, "Not a replica key");
    }
    let cond = match condition(&headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    // Bodies are held in memory while they are written: a few at a time
    // for one account, and so many megabytes across the hub.
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > hub.max_object) {
        return error(
            413,
            &format!("An object may be at most {} bytes", hub.max_object),
        );
    }
    let Ok(_upload) = hub.uploads_for(&d.account).acquire_owned().await else {
        return error(503, "The hub is closing");
    };
    let mib = declared
        .map_or(hub.max_object, |n| n.max(1))
        .div_ceil(1 << 20)
        .max(1);
    let Ok(_memory) = hub.memory.clone().acquire_many_owned(mib as u32).await else {
        return error(503, "The hub is closing");
    };
    let bytes: Bytes = match axum::body::to_bytes(req.into_body(), hub.max_object).await {
        Ok(b) => b,
        Err(_) => {
            return error(
                413,
                &format!("An object may be at most {} bytes", hub.max_object),
            );
        }
    };
    let size = bytes.len() as i64;
    // Reserve the room before writing, so uploads at once cannot all pass
    // the check; give it back if nothing is written.
    let (a, n) = (d.account.clone(), name.clone());
    let max = hub.max_bytes;
    let reserved = state
        .db
        .run(move |db| {
            let used = directory::usage(db, &a).1;
            if used + size > max {
                return Err(used);
            }
            directory::add_bytes(db, &a, &n, size);
            Ok(())
        })
        .await
        .unwrap_or(Err(0));
    if let Err(used) = reserved {
        return error(
            403,
            &format!(
                "This account holds {used} of its {max} bytes; delete a database or ask for more room"
            ),
        );
    }
    let unreserve = |state: &AppState| {
        let (a, n) = (d.account.clone(), name.clone());
        let db = state.db.clone();
        async move {
            let _ = db
                .run(move |db| directory::add_bytes(db, &a, &n, -size))
                .await;
        }
    };
    let lock = hub.lock_for(&format!("{}/{name}", d.account));
    let guard = lock.lock().await;
    // The database may have been deleted while this upload waited.
    let (a, n) = (d.account.clone(), name.clone());
    if !matches!(
        state
            .db
            .run(move |db| directory::database(db, &a, &n).is_some())
            .await,
        Ok(true)
    ) {
        drop(guard);
        unreserve(&state).await;
        return error(404, &format!("No database named {name}"));
    }
    let k = key.clone();
    let body = bytes.clone();
    let put = hub
        .store
        .with(&d.account, &name, move |r| {
            // Stores quote their entity tags differently: compare the bare
            // tags, then hand the store its own spelling, still checked by
            // the store and under this database's lock.
            let cond = match cond {
                Condition::IfMatch(want) => match block_on(r.get(&k)).map_err(|e| e.to_string())? {
                    Some(cur) if bare(&cur.etag) == bare(&want) => Condition::IfMatch(cur.etag),
                    _ => return Ok(Put::Refused),
                },
                other => other,
            };
            block_on(r.put(&k, &body[..], cond)).map_err(|e| e.to_string())
        })
        .await;
    drop(guard);
    if !matches!(put, Ok(Put::Written { .. })) {
        unreserve(&state).await;
    }
    match put {
        Ok(Put::Written { etag }) => {
            if key == moul_sync::layout::HEAD_KEY {
                let text = String::from_utf8_lossy(&bytes).replace(['\n', '\r'], "");
                let _ = hub.sender(&format!("{}/{name}", d.account)).send(text);
            }
            let mut r = axum::Json(json!({"etag": bare(&etag)})).into_response();
            r.headers_mut().insert(header::ETAG, etag_header(&etag));
            r
        }
        Ok(Put::Refused) => error(412, "The condition did not hold; nothing was written"),
        Err(e) => error(502, &format!("The store refused: {e}")),
    }
}

/// `DELETE /api/hub/db/:name/o/<key>`
async fn delete_object(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    Path((name, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let d = match replica_access(&state, &headers, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    if !valid_key(&key) {
        return error(400, "Not a replica key");
    }
    let k = key.clone();
    let gone = hub
        .store
        .with(&d.account, &name, move |r| {
            let size = block_on(r.list(&k))
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|o| o.key == k)
                .map_or(0, |o| o.size as i64);
            block_on(r.delete(&k)).map_err(|e| e.to_string())?;
            Ok(size)
        })
        .await;
    match gone {
        Ok(size) => {
            let (a, n) = (d.account.clone(), name.clone());
            let _ = state
                .db
                .run(move |db| directory::add_bytes(db, &a, &n, -size))
                .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => error(502, &format!("The store refused: {e}")),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    prefix: String,
}

/// `GET /api/hub/db/:name/list?prefix=`
async fn list_objects(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    Path(name): Path<String>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Response {
    let d = match replica_access(&state, &headers, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let prefix = q.prefix;
    match hub
        .store
        .with(&d.account, &name, move |r| block_on(r.list(&prefix)).map_err(|e| e.to_string()))
        .await
    {
        Ok(items) => axum::Json(json!({
            "items": items.iter().map(|o| json!({"key": o.key, "size": o.size, "modified": o.modified})).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => error(502, &format!("The store refused: {e}")),
    }
}

/// `GET /api/hub/db/:name/events`: `head` each time a device writes the
/// replica's head, and a comment every 15 seconds.
async fn events(
    State(state): State<AppState>,
    axum::Extension(hub): Ext,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let d = match replica_access(&state, &headers, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let rx = hub.sender(&format!("{}/{name}", d.account)).subscribe();
    let hello = format!("event: connected\ndata: {}\n\n", json!({"database": name}));
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(15),
        Duration::from_secs(15),
    );
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let s = stream::unfold(
        (Some(hello), rx, ping),
        |(hello, mut rx, mut ping)| async move {
            if let Some(h) = hello {
                return Some((Ok::<_, Infallible>(Bytes::from(h)), (None, rx, ping)));
            }
            loop {
                tokio::select! {
                    got = rx.recv() => match got {
                        Ok(head) => return Some((Ok(Bytes::from(format!("event: head\ndata: {head}\n\n"))), (None, rx, ping))),
                        // Fell behind: the next head says it all.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                    _ = ping.tick() => return Some((Ok(Bytes::from_static(b": ping\n\n")), (None, rx, ping))),
                }
            }
        },
    );
    let mut resp = Response::new(Body::from_stream(s));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

async fn require_admin_key(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    if moul_server::limits::has_admin_key(headers, &state.admin_key) {
        Ok(())
    } else {
        Err(error(401, "Admin authentication required"))
    }
}

/// `GET /api/hub/admin/accounts` (admin key)
async fn admin_accounts(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_admin_key(&state, &headers).await {
        return r;
    }
    match state.db.run(|db| directory::accounts(db)).await {
        Ok(items) => axum::Json(json!({"items": items})).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `POST /api/hub/admin/accounts` with `{email}` (admin key): an account
/// that can sign in whatever the sign-up setting.
async fn admin_create_account(State(state): State<AppState>, req: Request) -> Response {
    if let Err(r) = require_admin_key(&state, req.headers()).await {
        return r;
    }
    let email = text(&body_json(req).await, "email").to_ascii_lowercase();
    if !plausible_email(&email) {
        return error(400, "Give an email address");
    }
    let out = state
        .db
        .run(move |db| {
            if let Some(a) = directory::account_by_email(db, &email) {
                return Ok(a.to_json());
            }
            let r = db.handle(
                moul_core::Request::post(
                    &format!("/api/moul/{ACCOUNTS}/records"),
                    json!({"email": email}),
                )
                .owner(),
            );
            if r.status >= 300 {
                return Err(r);
            }
            Ok(directory::account_by_email(db, &email)
                .map(|a| a.to_json())
                .unwrap_or(Value::Null))
        })
        .await;
    match out {
        Ok(Ok(v)) => status_json(201, v),
        Ok(Err(r)) => to_http(r, None),
        Err(e) => e.into_response(),
    }
}

/// `POST /api/hub/admin/invites` with `{uses?}` (admin key)
async fn admin_invite(State(state): State<AppState>, req: Request) -> Response {
    if let Err(r) = require_admin_key(&state, req.headers()).await {
        return r;
    }
    let uses = body_json(req).await["uses"].as_i64().unwrap_or(1);
    match state
        .db
        .run(move |db| directory::create_invite(db, uses))
        .await
    {
        Ok(Ok(code)) => status_json(201, json!({"code": code, "uses": uses.max(1)})),
        Ok(Err(e)) => error(500, &e),
        Err(e) => e.into_response(),
    }
}

/// Compacts databases that were pushed to, every minute: level 1 every 5
/// minutes, level 2 every hour, a snapshot a day, 7 days of history.
pub fn spawn_maintenance(state: AppState, hub: Arc<Hub>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            if !state.db.is_open() {
                return;
            }
            maintain_due(&state, &hub).await;
        }
    });
}

/// One maintenance pass over the databases that are due. Returns how many
/// were maintained.
pub async fn maintain_due(state: &AppState, hub: &Hub) -> usize {
    let due = state
        .db
        .run(|db| directory::due_for_maintenance(db, 5 * 60 * 1000, 20))
        .await
        .unwrap_or_default();
    let mut done = 0;
    for (account, name) in due {
        // Under the database's lock, and only while it still exists, so a
        // deleted database does not come back with compacted files.
        let lock = hub.lock_for(&format!("{account}/{name}"));
        let _guard = lock.lock().await;
        let (a, n) = (account.clone(), name.clone());
        if !matches!(
            state
                .db
                .run(move |db| directory::database(db, &a, &n).is_some())
                .await,
            Ok(true)
        ) {
            continue;
        }
        let report = hub
            .store
            .with(&account, &name, |r| {
                let head = block_on(r.get(moul_sync::layout::HEAD_KEY))
                    .ok()
                    .flatten()
                    .and_then(|o| serde_json::from_slice::<moul_sync::layout::Head>(&o.bytes).ok());
                if let Some(h) = head {
                    block_on(moul_sync::maintain::maintain(
                        r,
                        moul_ltx::Txid(h.txid),
                        None,
                        &moul_sync::Retention::default(),
                        moul_vfs::system_clock(),
                    ))
                    .map_err(|e| e.to_string())?;
                }
                let bytes: i64 = block_on(r.list(""))
                    .map_err(|e| e.to_string())?
                    .iter()
                    .map(|o| o.size as i64)
                    .sum();
                Ok(bytes)
            })
            .await;
        match report {
            Ok(bytes) => {
                done += 1;
                let _ = state
                    .db
                    .run(move |db| directory::maintained(db, &account, &name, bytes))
                    .await;
            }
            Err(e) => eprintln!("moul hub: compacting {account}/{name}: {e}"),
        }
    }
    done
}
