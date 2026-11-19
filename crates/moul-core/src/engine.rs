//! The engine: takes a request and returns a response.

use moul_store::schema::Collection;
use moul_store::{Store, catalog};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Caller, Request, Response};
use crate::auth::OtpThrottle;
use crate::budget::{Budget, Meter, exceeded};
use crate::changes::{Change, ChangeInfo};
use crate::files::Blobs;
use crate::guard::SqlGuard;
use crate::host::Host;
use crate::realtime::{Delivery, Realtime, RecordEvent, Subscription, SubscriptionId};
use crate::router::{Route, route};
use crate::{
    admin, auth, collections, crypto, dataio, definition, effects, files, flags, ids, oauth,
    records, rules, settings, time, webhooks, workers,
};

/// Settings of an engine. The defaults suit a builder's own tab.
#[derive(Clone, Debug)]
pub struct Config {
    /// A key that makes a request the owner's when sent as `X-Admin-Key`
    /// or as a bearer token, like the Go engine's admin key. `None` in a
    /// tab, where the host marks the owner's requests itself.
    pub admin_key: Option<String>,
    /// The budget of every request.
    pub budget: Budget,
    /// How long a sign-in token lasts.
    pub token_ttl_ms: i64,
    /// How long after expiry a token may still be refreshed.
    pub refresh_grace_ms: i64,
    /// The address links in mail point at.
    pub public_url: String,
    /// How long a signed file URL lasts.
    pub file_url_ttl_ms: i64,
    /// The largest upload accepted, in bytes.
    pub max_upload_bytes: usize,
    /// The key that signs tokens and file URLs. `None` uses (and on first
    /// open makes) a random key kept in the writer-only store.
    pub signing_key: Option<Vec<u8>>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            admin_key: None,
            budget: Budget::default(),
            token_ttl_ms: 24 * 3600 * 1000,
            refresh_grace_ms: 7 * 24 * 3600 * 1000,
            public_url: "http://localhost:8090".into(),
            file_url_ttl_ms: 3600 * 1000,
            max_upload_bytes: 32 << 20,
            signing_key: None,
        }
    }
}

/// What can go wrong opening an engine.
#[derive(Debug)]
pub enum OpenError {
    /// The stores could not be opened or migrated.
    Store(moul_store::Error),
    /// SQLite failed while reading engine state.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Store(e) => write!(f, "{e}"),
            OpenError::Sqlite(e) => write!(f, "SQLite: {e}"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<moul_store::Error> for OpenError {
    fn from(e: moul_store::Error) -> Self {
        OpenError::Store(e)
    }
}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        OpenError::Sqlite(e)
    }
}

/// Whether this engine may write the shared store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// It holds the writer role: it applies requests.
    Writer,
    /// It follows another writer: it answers reads, and refuses writes.
    Follower,
}

/// Who a host's caller is, as [`Engine::identity`] tells it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// The owner: the admin key, the host's word, or a root user.
    pub admin: bool,
    /// The signed-in person, or root user.
    pub user: Option<auth::User>,
    /// A token was sent and refused.
    pub rejected: bool,
}

/// The collections, cached while `_moul_version` is unchanged.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    version: i64,
    all: Vec<Collection>,
    loaded: bool,
    /// While a read is served, the collections it looked up: the read set
    /// a live query refreshes on. See [`Engine::handle`].
    reads: std::cell::RefCell<Option<Reads>>,
}

/// The collections one read looked up, by id, or `all` when it may depend
/// on any table (a rule that runs `exists(` or `get(`).
#[derive(Debug, Default)]
pub(crate) struct Reads {
    ids: std::collections::BTreeSet<String>,
    all: bool,
    /// The collections whose rules were scanned for what they read.
    scanned: std::collections::BTreeSet<String>,
}

impl Reads {
    /// The `Moul-Reads` header: `*`, or collection ids joined by commas.
    fn header(&self) -> String {
        if self.all {
            "*".into()
        } else {
            self.ids.iter().cloned().collect::<Vec<_>>().join(",")
        }
    }
}

/// Whether a rule or filter may read tables its collections do not name.
fn reads_any_table(rule: &str) -> bool {
    rule.contains("exists(") || rule.contains("get(")
}

impl Catalog {
    pub(crate) fn refresh(&mut self, conn: &Connection) -> ApiResult<()> {
        let v = catalog::version(conn)?;
        if !self.loaded || v != self.version {
            self.all = catalog::load_all(conn)?;
            self.version = v;
            self.loaded = true;
        }
        Ok(())
    }

    pub(crate) fn all(&self) -> &[Collection] {
        &self.all
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Collection> {
        let found = self.all.iter().find(|c| c.name == name);
        if let Some(c) = found {
            self.note_read(c);
        }
        found
    }

    /// Adds `c` to the read set, if one is being recorded, with the
    /// collections its rules read through `@collection.<name>`: those
    /// conditions query their tables directly, without a lookup here.
    fn note_read(&self, c: &Collection) {
        let mut reads = self.reads.borrow_mut();
        let Some(reads) = reads.as_mut() else {
            return;
        };
        if !reads.scanned.insert(c.id.clone()) {
            return;
        }
        reads.ids.insert(c.id.clone());
        let r = &c.rules;
        let rules = [
            &r.list_rule,
            &r.view_rule,
            &r.create_rule,
            &r.update_rule,
            &r.delete_rule,
            &r.subscribe_rule,
        ];
        reads.all |= rules.iter().any(|rule| reads_any_table(rule));
        for rule in rules {
            for name in moul_rules::collections_named(rule) {
                if let Some(other) = self.all.iter().find(|o| o.name == name) {
                    reads.ids.insert(other.id.clone());
                }
            }
        }
    }

    fn start_reads(&self) {
        *self.reads.borrow_mut() = Some(Reads::default());
    }

    fn take_reads(&self) -> Option<Reads> {
        self.reads.borrow_mut().take()
    }

    pub(crate) fn by_id(&self, id: &str) -> Option<&Collection> {
        self.all.iter().find(|c| c.id == id)
    }
}

/// Who sent a request.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Who {
    /// Nobody signed in. `rejected` is set when a token was sent but not
    /// accepted.
    Guest { rejected: bool },
    /// The owner: the host vouched, or the admin key or a root user token
    /// was sent.
    Owner { root: Option<auth::User> },
    /// A person signed in to an auth collection.
    User(auth::User),
}

impl Who {
    pub(crate) fn is_admin(&self) -> bool {
        matches!(self, Who::Owner { .. })
    }

    /// The auth map rules see as `@request.auth`: `id`, `email`,
    /// `username` and `moul`, or nothing for a guest or the admin key.
    pub(crate) fn auth_map(&self) -> Option<Map<String, Value>> {
        match self {
            Who::User(u) | Who::Owner { root: Some(u) } => Some(u.map()),
            _ => None,
        }
    }

    pub(crate) fn user(&self) -> Option<&auth::User> {
        match self {
            Who::User(u) => Some(u),
            _ => None,
        }
    }

    /// The actor recorded in the change log.
    pub(crate) fn actor(&self, catalog: &Catalog) -> String {
        match self {
            Who::Guest { .. } => String::new(),
            Who::Owner { root: None } => "owner".into(),
            Who::Owner { root: Some(u) } => format!("_rootUsers/{}", u.id),
            Who::User(u) => {
                let c = catalog
                    .get(&u.collection)
                    .map(|c| c.id.as_str())
                    .unwrap_or(&u.collection);
                format!("{c}/{}", u.id)
            }
        }
    }

    pub(crate) fn rejected(&self) -> bool {
        matches!(self, Who::Guest { rejected: true })
    }
}

/// Everything a handler may use while it serves one request.
pub(crate) struct Cx<'a> {
    pub conn: &'a Connection,
    pub host: &'a mut dyn Host,
    pub blobs: &'a mut dyn Blobs,
    pub catalog: &'a mut Catalog,
    pub config: &'a Config,
    pub key: &'a [u8],
    pub who: Who,
    pub req: &'a Request,
    pub now_ms: i64,
    pub meter: &'a Meter,
    pub otp: &'a mut OtpThrottle,
    /// Fences SQL that comes from rules into the shared store.
    pub guard: &'a SqlGuard,
    /// Record changes to announce once the transaction commits.
    pub events: Vec<RecordEvent>,
    /// What a write changed, for the change log.
    pub change: Option<ChangeInfo>,
    /// Records a write changed besides its own: cascaded deletes and
    /// cleared references, as (collection id, record id, operation).
    pub effects: Vec<(String, String, String)>,
    /// Blobs written by this request, removed again if it rolls back.
    pub new_blobs: Vec<String>,
    /// Blobs to remove once the request commits.
    pub dead_blobs: Vec<String>,
}

impl Cx<'_> {
    /// The current time as the Go engine stamps it.
    pub fn now(&self) -> String {
        time::rfc3339(self.now_ms)
    }

    /// The collection named `name`, refreshed from the store if another
    /// request or device changed the definitions.
    pub fn collection(&mut self, name: &str) -> ApiResult<Option<Collection>> {
        self.catalog.refresh(self.conn)?;
        Ok(self.catalog.get(name).cloned())
    }

    /// Every collection.
    pub fn collections(&mut self) -> ApiResult<Vec<Collection>> {
        self.catalog.refresh(self.conn)?;
        Ok(self.catalog.all().to_vec())
    }

    /// `@request` as rules see it: method, query, headers and body.
    pub fn request_context(&self, body: Option<&Map<String, Value>>) -> Map<String, Value> {
        let mut headers = Map::new();
        for (k, v) in &self.req.headers {
            let key = k.to_ascii_lowercase().replace('-', "_");
            headers
                .entry(key)
                .or_insert_with(|| Value::String(v.clone()));
        }
        let mut query = Map::new();
        for (k, v) in &self.req.query {
            query
                .entry(k.clone())
                .or_insert_with(|| Value::String(v.clone()));
        }
        let mut ctx = Map::new();
        ctx.insert(
            "body".into(),
            Value::Object(body.cloned().unwrap_or_default()),
        );
        ctx.insert("headers".into(), Value::Object(headers));
        ctx.insert("query".into(), Value::Object(query));
        ctx.insert("method".into(), Value::String(self.req.method.clone()));
        ctx
    }

    /// The owner-only gate of the Go engine's admin surface.
    pub fn require_admin(&self) -> ApiResult<()> {
        if self.who.is_admin() {
            return Ok(());
        }
        if self.who.auth_map().is_some() {
            return Err(ApiError::forbidden("Root or admin access required"));
        }
        Err(ApiError::unauthorized(
            "Admin authentication required (root user token or admin key)",
        ))
    }

    /// The gate of the Go engine's setup and admin-login endpoints: the
    /// admin key (or the host's owner) only; a root user's token is not
    /// enough.
    pub fn require_admin_key(&self) -> ApiResult<()> {
        if matches!(self.who, Who::Owner { root: None }) {
            return Ok(());
        }
        Err(ApiError::unauthorized("Admin authentication required"))
    }

    /// Records what this write changed.
    pub fn changed(&mut self, collection: &Collection, record: Option<&str>, fields: Vec<String>) {
        self.change = Some(ChangeInfo {
            collection: Some(collection.id.clone()),
            record: record.map(str::to_owned),
            fields,
            effects: Vec::new(),
        });
    }
}

/// The Moul engine over one database.
pub struct Engine {
    store: Store,
    host: Box<dyn Host>,
    blobs: Box<dyn Blobs>,
    config: Config,
    catalog: Catalog,
    device: String,
    key: Vec<u8>,
    role: Role,
    realtime: Realtime,
    otp: OtpThrottle,
    guard: SqlGuard,
    last_seq: i64,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("device", &self.device)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

fn device_value(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM device._device WHERE key = ?1",
        [key],
        |r| r.get(0),
    )
    .optional()
}

fn set_device_value(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO device._device (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

impl Engine {
    /// Opens the engine as the writer: migrates all three stores (converting
    /// a Go engine database), and makes this device's id and the signing key
    /// if they do not exist yet.
    pub fn open(
        store: Store,
        host: impl Host + 'static,
        blobs: impl Blobs + 'static,
        config: Config,
    ) -> Result<Engine, OpenError> {
        Engine::open_with_role(store, Box::new(host), Box::new(blobs), config, Role::Writer)
    }

    /// Opens the engine on a device that follows another writer: only the
    /// writer-only and device stores are migrated, and writes are refused.
    pub fn open_follower(
        store: Store,
        host: impl Host + 'static,
        blobs: impl Blobs + 'static,
        config: Config,
    ) -> Result<Engine, OpenError> {
        Engine::open_with_role(
            store,
            Box::new(host),
            Box::new(blobs),
            config,
            Role::Follower,
        )
    }

    fn open_with_role(
        mut store: Store,
        mut host: Box<dyn Host>,
        blobs: Box<dyn Blobs>,
        config: Config,
        role: Role,
    ) -> Result<Engine, OpenError> {
        match role {
            Role::Writer => {
                store.migrate(|| ids::field_id(&mut *host))?;
            }
            Role::Follower => store.migrate_local()?,
        }
        let device = match device_value(store.conn(), "id")? {
            Some(id) => id,
            None => {
                let id = format!("d{}", ids::random_chars(&mut *host, 11));
                set_device_value(store.conn(), "id", &id)?;
                id
            }
        };
        let key = match &config.signing_key {
            Some(k) => k.clone(),
            None => {
                let stored: Option<Vec<u8>> = store
                    .conn()
                    .query_row(
                        "SELECT value FROM writer._keys WHERE name = 'token'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                match stored {
                    Some(k) => k,
                    None => {
                        let mut k = vec![0u8; 32];
                        host.fill_random(&mut k);
                        store.conn().execute(
                            "INSERT INTO writer._keys (name, value) VALUES ('token', ?1)",
                            [&k],
                        )?;
                        k
                    }
                }
            }
        };
        let last_seq = if role == Role::Writer || store.format()? > 0 {
            store
                .conn()
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM main._changes", [], |r| {
                    r.get(0)
                })
                .unwrap_or(0)
        } else {
            0
        };
        // The fence around rule SQL goes on every connection the store
        // opens, including after an import or a replica apply.
        let guard = SqlGuard::default();
        let hook = guard.clone();
        store.on_connect(move |conn| hook.install_on(conn))?;
        Ok(Engine {
            store,
            host,
            blobs,
            config,
            catalog: Catalog::default(),
            device,
            key,
            role,
            realtime: Realtime::default(),
            otp: OtpThrottle::default(),
            guard,
            last_seq,
        })
    }

    /// This device's id, the first part of the request ids it makes.
    pub fn device_id(&self) -> &str {
        &self.device
    }

    /// Whether this engine writes or follows.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The store underneath.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The store underneath, for a host that applies replica pages or runs
    /// the owner's raw SQL. Writes made here bypass the change log.
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// Runs `f` on the store while SQLite's authorizer lets the statements
    /// prepared or run in it only read: tables of any store and the values
    /// of pragmas, but nothing written or set, no database attached or
    /// detached, and no transaction begun or ended. A refused statement
    /// fails with `SQLITE_AUTH` ("not authorized"). For a host that runs
    /// the owner's queries.
    pub fn read_only<T>(&self, f: impl FnOnce(&Store) -> T) -> T {
        self.guard.read_only(|| f(&self.store))
    }

    /// Replaces the shared store with a database file, such as one
    /// [`Store::export`] wrote or one the Go engine made (converted here),
    /// and migrates it. The writer-only and device stores are kept.
    /// Refused on a follower, whose shared store is the writer's.
    ///
    /// A copy of the file is migrated in memory first, so a file that
    /// cannot be migrated (one from a newer engine, say) is refused and the
    /// shared store keeps its data.
    pub fn import_database(&mut self, bytes: &[u8]) -> Result<(), OpenError> {
        if self.role == Role::Follower {
            return Err(OpenError::Store(moul_store::Error::BadImport(
                "this device follows another writer; take over writing before importing".into(),
            )));
        }
        {
            let mut scratch = Store::open_in_memory()?;
            scratch.import(bytes)?;
            // Not the host's ids: the real migration below takes those.
            let mut n = 0u32;
            scratch.migrate(|| {
                n += 1;
                format!("fld_{n}")
            })?;
        }
        self.store.import(bytes)?;
        let host = &mut self.host;
        let migrated = self.store.migrate(|| ids::field_id(&mut **host));
        // The shared store was replaced, migrated or not.
        self.catalog = Catalog::default();
        self.last_seq = self
            .store
            .conn()
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM main._changes", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        migrated?;
        Ok(())
    }

    /// Closes the engine and hands back the store.
    pub fn into_store(self) -> Store {
        self.store
    }

    /// The host, for tests that move its clock.
    pub fn host_mut(&mut self) -> &mut dyn Host {
        &mut *self.host
    }

    /// The settings.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Changes this engine's role. A device that takes over writing calls
    /// this after the hand-off; it then migrates the shared store if needed.
    pub fn set_role(&mut self, role: Role) -> Result<(), OpenError> {
        if role == Role::Writer && self.role == Role::Follower {
            let host = &mut self.host;
            self.store.migrate(|| ids::field_id(&mut **host))?;
        }
        self.role = role;
        Ok(())
    }

    /// Registers a live subscription and returns its id. Matching record
    /// changes come out of [`take_deliveries`](Self::take_deliveries).
    pub fn subscribe(&mut self, sub: Subscription) -> Result<SubscriptionId, Response> {
        let who = self.identify_headers(&sub.credentials, sub.owner);
        if let Some(r) = who.as_ref().err() {
            return Err(r.clone().into_response());
        }
        self.catalog
            .refresh(self.store.conn())
            .map_err(ApiError::into_response)?;
        self.realtime.subscribe(sub, who.unwrap(), &self.catalog)
    }

    /// Tells the engine which records a subscriber already has, such as
    /// the rows of the list the host loaded when it subscribed. On a
    /// follower, a delete it learns of by id reaches only subscribers that
    /// had the record. Returns false for an unknown subscription.
    pub fn note_visible(&mut self, id: SubscriptionId, records: &[String]) -> bool {
        self.realtime.note_visible(id, records)
    }

    /// Ends a subscription.
    pub fn unsubscribe(&mut self, id: SubscriptionId) -> bool {
        self.realtime.unsubscribe(id)
    }

    /// Record changes for live subscriptions since the last call, each
    /// already checked against and redacted for its subscriber.
    pub fn take_deliveries(&mut self) -> Vec<Delivery> {
        self.realtime.take()
    }

    /// Subscriptions the engine ended since the last call, with the reason:
    /// the token was revoked or expired, the admin key changed, or the
    /// user was deleted. The host closes their streams.
    pub fn take_closed(&mut self) -> Vec<(SubscriptionId, String)> {
        self.realtime.take_closed()
    }

    /// Ends every live subscription with `reason`; they come out of
    /// [`take_closed`](Self::take_closed). For a host whose data changed in
    /// a way the change log does not describe event by event, such as a
    /// snapshot from the replica or an import: subscribers cannot know what
    /// happened in between and must load the data again.
    pub fn end_subscriptions(&mut self, reason: &str) {
        self.realtime.retain(|_, _| Some(reason.to_owned()));
    }

    /// Reads the change log after `seq` and announces the changes to live
    /// subscriptions. A follower calls this after pages from the writer were
    /// applied underneath SQLite, which fires no other change events.
    pub fn catch_up(&mut self) -> Vec<Change> {
        let conn = self.store.conn();
        let changes = crate::changes::since(conn, self.last_seq, 10_000).unwrap_or_default();
        if let Some(last) = changes.last() {
            self.last_seq = last.seq;
        }
        if self.catalog.refresh(conn).is_ok() {
            let events = crate::changes::events_for(conn, &self.catalog, &changes);
            let now = self.host.now_ms();
            self.after_commit(events, now);
        }
        changes
    }

    /// The last change-log position this engine announced. A host that
    /// keeps it across restarts hands it back with
    /// [`set_seen`](Self::set_seen).
    pub fn seen(&self) -> i64 {
        self.last_seq
    }

    /// Sets the change-log position [`catch_up`](Self::catch_up) reads from.
    pub fn set_seen(&mut self, seq: i64) {
        self.last_seq = seq;
    }

    /// The change log after `seq`, oldest first, at most `limit` entries.
    pub fn changes(&self, after: i64, limit: usize) -> Vec<Change> {
        crate::changes::since(self.store.conn(), after, limit).unwrap_or_default()
    }

    /// Who the credentials in `headers` name, for a host that hands out
    /// tickets or checks access before it calls the engine.
    pub fn identity(
        &mut self,
        headers: &[(String, String)],
        remote: Option<&str>,
    ) -> Result<Identity, Response> {
        let mut req = Request::new("GET", "/");
        req.headers = headers.to_vec();
        req.remote = remote.map(str::to_owned);
        let now = self.host.now_ms();
        match auth::identify(self.store.conn(), &self.config, &self.key, &req, now) {
            Ok(Who::Owner { root }) => Ok(Identity {
                admin: true,
                user: root,
                rejected: false,
            }),
            Ok(Who::User(u)) => Ok(Identity {
                admin: false,
                user: Some(u),
                rejected: false,
            }),
            Ok(Who::Guest { rejected }) => Ok(Identity {
                admin: false,
                user: None,
                rejected,
            }),
            Err(e) => Err(e.into_response()),
        }
    }

    /// Ends live subscriptions whose credentials no longer hold (a token
    /// expired or was revoked, a user was deleted). The engine checks after
    /// each write; a host calls this from time to time as well, so a quiet
    /// collection's subscribers are dropped when their token expires.
    pub fn check_subscriptions(&mut self) {
        if self.catalog.refresh(self.store.conn()).is_ok() {
            let now = self.host.now_ms();
            self.recheck_subscriptions(now);
        }
    }

    /// The settings, stored values over defaults, secrets included: for
    /// the host, never for an answer.
    pub fn settings(&self) -> std::collections::BTreeMap<String, String> {
        settings::load(self.store.conn()).unwrap_or_default()
    }

    /// Checks the `state` an OAuth provider sent back: made by this engine,
    /// not expired. Gives the collection, the provider and the PKCE verifier.
    pub fn verify_oauth_state(&mut self, state: &str) -> Option<oauth::State> {
        let now = self.host.now_ms();
        oauth::verify_state(&self.key, state, now)
    }

    /// Signs in the person an OAuth provider vouched for, into the auth
    /// collection `collection`: as `POST /api/moul/:name/auth-with-password`
    /// answers. The host checked the provider's answer; this finds the
    /// linked account, links one by a verified email, or makes one.
    pub fn sign_in_external(
        &mut self,
        collection: &str,
        profile: oauth::ExternalProfile,
        remote: Option<&str>,
    ) -> Response {
        let route = Route::ExternalSignIn(collection.to_owned(), profile);
        let mut req = Request::new("POST", &format!("/api/moul/{collection}/auth-with-oauth2"));
        req.remote = remote.map(str::to_owned);
        self.serve(&route, &req)
    }

    /// Runs one `SELECT` over the shared store only, for an agent: the
    /// fence that holds rule SQL refuses the writer-only and device stores
    /// (credentials, keys), writes, pragmas and attaching. At most
    /// `max_rows` rows come back. Answers the column names and the rows.
    pub fn query_shared(
        &self,
        sql: &str,
        max_rows: usize,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
        let conn = self.store.conn();
        let run = || -> rusqlite::Result<(Vec<String>, Vec<Vec<Value>>)> {
            let mut stmt = conn.prepare(sql)?;
            let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
            let n = names.len();
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                if out.len() >= max_rows {
                    break;
                }
                let mut row = Vec::with_capacity(n);
                for i in 0..n {
                    row.push(match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(v) => json!(v),
                        rusqlite::types::ValueRef::Real(v) => json!(v),
                        rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                        rusqlite::types::ValueRef::Blob(b) => json!(format!("<{} bytes>", b.len())),
                    });
                }
                out.push(row);
            }
            Ok((names, out))
        };
        match self.guard.run(conn, run) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) | Err(e) => Err(match e {
                rusqlite::Error::SqliteFailure(_, Some(m)) => m,
                other => other.to_string(),
            }),
        }
    }

    /// A fresh token for root user `id`, if they still exist: for a host
    /// whose own sign-in (an OAuth refresh token) vouches for them.
    pub fn issue_root_token(&mut self, id: &str) -> Option<String> {
        let row: (String, String) = self
            .store
            .conn()
            .query_row(
                "SELECT email, username FROM writer._rootUsers WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten()?;
        let now_s = self.host.now_ms().div_euclid(1000);
        let claims = crypto::Claims {
            id: id.to_owned(),
            email: row.0,
            username: row.1,
            collection: auth::ROOT_COLLECTION.to_owned(),
            exp: Some(now_s + self.config.token_ttl_ms / 1000),
            iat: Some(now_s),
            jti: ids::random_hex(&mut *self.host, 16),
        };
        Some(crypto::sign_token(&self.key, &claims))
    }

    /// A webhook's URL and secret, for a host that tests it: `None` if the
    /// collection or the webhook does not exist.
    pub fn webhook_target(
        &mut self,
        collection: &str,
        id: &str,
    ) -> Option<(String, Option<String>)> {
        self.catalog.refresh(self.store.conn()).ok()?;
        let c = self.catalog.all().iter().find(|c| c.name == collection)?;
        let hook = c
            .webhooks
            .iter()
            .find(|h| h.get("id").and_then(Value::as_str) == Some(id))?;
        let url = hook.get("url").and_then(Value::as_str)?.to_owned();
        Some((url, webhooks::secret(self.store.conn(), &c.id, id)))
    }

    /// Runs `f` in a write transaction of its own, for the effect calls.
    fn effect<T: Default>(
        &mut self,
        f: impl FnOnce(&Connection, &mut dyn Host, i64) -> rusqlite::Result<T>,
    ) -> T {
        if self.role == Role::Follower {
            return T::default();
        }
        let now = self.host.now_ms();
        let host = &mut *self.host;
        self.store.write(|tx| f(tx, host, now)).unwrap_or_default()
    }

    /// Takes up to `limit` mail that is due, leasing it; see [`effects`].
    pub fn due_mail(&mut self, limit: usize) -> Vec<effects::Mail> {
        self.effect(|c, _, now| effects::due_mail(c, now, limit))
    }

    /// Reports how sending mail `id` went.
    pub fn mail_done(&mut self, id: &str, outcome: Result<(), String>) {
        self.effect(|c, _, now| effects::mail_done(c, id, outcome, now))
    }

    /// Takes up to `limit` webhook calls that are due, leasing them.
    pub fn due_webhooks(&mut self, limit: usize) -> Vec<effects::WebhookCall> {
        self.effect(|c, _, now| effects::due_webhooks(c, now, limit))
    }

    /// Reports how webhook call `id` went.
    pub fn webhook_done(&mut self, id: &str, outcome: Result<(), String>) {
        self.effect(|c, _, now| effects::webhook_done(c, id, outcome, now))
    }

    /// Claims up to `limit` due jobs for `workers`, as runner `node`.
    pub fn claim_jobs(&mut self, workers: &[&str], limit: usize, node: &str) -> Vec<workers::Job> {
        self.effect(|c, _, now| workers::claim(c, workers, limit, node, now))
    }

    /// Reports how job `id` went: done, or the error (retried or discarded).
    pub fn job_done(&mut self, id: &str, outcome: Result<(), String>) {
        self.effect(|c, host, now| workers::finish(c, host, id, outcome, now))
    }

    /// Adds a job, as `POST /api/workers` does.
    pub fn enqueue_job(&mut self, fields: Map<String, Value>) -> Result<Value, Response> {
        if self.role == Role::Follower {
            return Err(ApiError::new(409, "This device follows another writer").into_response());
        }
        let now = self.host.now_ms();
        let host = &mut self.host;
        let mut out = Err(ApiError::internal());
        let _ = self.store.write(|tx| -> rusqlite::Result<()> {
            out = workers::enqueue(tx, &mut **host, now, &fields);
            Ok(())
        });
        out.map_err(ApiError::into_response)
    }

    /// Housekeeping: jobs whose runner stopped go back to `available`
    /// after `lease_ms`, and finished effects older than a week are
    /// deleted. Returns how many rows changed.
    pub fn tidy_effects(&mut self, lease_ms: i64) -> usize {
        self.effect(|c, _, now| Ok(workers::rescue(c, lease_ms, now)? + effects::prune(c, now)?))
    }

    /// What is waiting to be sent or run: `{mail, webhooks, jobs}`.
    pub fn pending_effects(&self) -> Value {
        effects::pending(self.store.conn()).unwrap_or(Value::Null)
    }

    fn identify_headers(
        &mut self,
        headers: &[(String, String)],
        owner: bool,
    ) -> Result<Who, ApiError> {
        let mut req = Request::new("GET", "/");
        req.headers = headers.to_vec();
        if owner {
            req.caller = Caller::Owner;
        }
        let now = self.host.now_ms();
        auth::identify(self.store.conn(), &self.config, &self.key, &req, now)
    }

    /// Serves one request.
    ///
    /// A successful `GET` answers with `Moul-Reads`: the ids of the
    /// collections it read, comma-separated, or `*` when a rule it ran may
    /// read any table. A live query refreshes when a change touches one of
    /// them (or any definition changes); see [`changes`](Self::changes).
    pub fn handle(&mut self, req: Request) -> Response {
        let route = route(&req);
        let reads = req.method == "GET" || req.method == "HEAD";
        if reads {
            self.catalog.start_reads();
        }
        let mut response = self.serve(&route, &req);
        if let Some(mut set) = self.catalog.take_reads().filter(|_| reads)
            && response.status < 400
        {
            set.all |= reads_any_table(req.query_param("filter"));
            response.headers.push(("Moul-Reads".into(), set.header()));
        }
        response
    }

    fn serve(&mut self, route: &Route, req: &Request) -> Response {
        match route {
            Route::NotFound => return ApiError::not_found("Not Found").into_response(),
            Route::MethodNotAllowed => {
                return ApiError::new(405, "Method Not Allowed").into_response();
            }
            _ => {}
        }
        // Credentials in the query string end up in logs; the Go engine
        // refuses them outright.
        if !req.query_param("token").is_empty() {
            return ApiError::bad_request("Authentication via ?token= query parameter is disabled for security reasons. Provide your credentials via Authorization header (Bearer <token>) or request a single-use ticket via POST /api/realtime/ticket.").into_response();
        }
        if ["admin_key", "adminKey", "api_key"]
            .iter()
            .any(|k| !req.query_param(k).is_empty())
        {
            return ApiError::bad_request("Authentication via URL query parameters is disabled for security reasons. Provide your admin key via the X-Admin-Key header or request a single-use ticket via POST /api/realtime/ticket.").into_response();
        }

        let now_ms = self.host.now_ms();
        let who = match auth::identify(self.store.conn(), &self.config, &self.key, req, now_ms) {
            Ok(w) => w,
            Err(e) => return e.into_response(),
        };
        // A signed-in caller's own record feeds `@request.auth`, so a read
        // depends on it.
        if let Some(u) = who.user() {
            let _ = self.catalog.refresh(self.store.conn());
            self.catalog.get(&u.collection);
        }
        let logged = route.is_logged_write();
        let writes = req.method != "GET" && req.method != "HEAD";
        if writes && self.role == Role::Follower {
            return ApiError::new(
                409,
                "This device follows another writer; take over writing to make changes",
            )
            .into_response();
        }

        let mut budget = self.config.budget;
        let header = req.header_value("Moul-Budget");
        if !header.is_empty() {
            budget = Budget::parse_header(header, budget);
        }

        // A write sent again with the same id is answered from the change
        // log and applied only once.
        let explicit_id = req.id.clone().or_else(|| {
            Some(req.header_value("Moul-Request-Id").to_owned()).filter(|s| !s.is_empty())
        });
        if logged && let Some(id) = &explicit_id {
            if id.len() > 128 || id.chars().any(|c| c.is_control()) {
                return ApiError::bad_request(
                    "Moul-Request-Id must be at most 128 printable characters",
                )
                .into_response();
            }
            // This device's own ids come from its counter; only the owner,
            // resending what the engine gave it, may name one.
            let own = id
                .rsplit_once(':')
                .is_some_and(|(device, _)| device == self.device);
            if own && !who.is_admin() {
                return ApiError::bad_request(
                    "Moul-Request-Id names an id this device makes itself; choose another",
                )
                .into_response();
            }
            // Every guest is the same actor, so one guest could claim or
            // replay another's id. Until devices sign their ids (Phase 9),
            // only signed-in senders and the owner may name one.
            if matches!(who, Who::Guest { .. }) {
                return ApiError::unauthorized("Sign in to send a Moul-Request-Id").into_response();
            }
            match crate::changes::find(self.store.conn(), id) {
                Ok(Some(done)) => return self.replay(route, req, who, done),
                Ok(None) => {}
                Err(e) => return e.into_response(),
            }
        }

        let begin = if writes { "BEGIN IMMEDIATE" } else { "BEGIN" };
        if let Err(e) = self.store.conn().execute_batch(begin) {
            return ApiError::from(e).into_response();
        }
        let request_id = if logged {
            match explicit_id {
                Some(id) => Some(id),
                None => match self.next_request_id() {
                    Ok(id) => Some(id),
                    Err(e) => {
                        let _ = self.store.conn().execute_batch("ROLLBACK");
                        return e.into_response();
                    }
                },
            }
        } else {
            None
        };
        let clock = self.host.statement_clock();
        let meter = Meter::start(self.store.conn_mut(), budget, clock);
        let conn = self.store.conn();
        if logged {
            let _ = conn.execute_batch("SAVEPOINT op");
        }

        let mut cx = Cx {
            conn,
            host: &mut *self.host,
            blobs: &mut *self.blobs,
            catalog: &mut self.catalog,
            config: &self.config,
            key: &self.key,
            who,
            req,
            now_ms,
            meter: &meter,
            otp: &mut self.otp,
            guard: &self.guard,
            events: Vec::new(),
            change: None,
            effects: Vec::new(),
            new_blobs: Vec::new(),
            dead_blobs: Vec::new(),
        };
        let mut result = dispatch(&mut cx, route);
        // A statement the budget stopped fails the whole request, even when
        // the handler swallowed the error (a rule lookup reads as "no row").
        if let Some(which) = meter.interrupted() {
            result = Err(exceeded(which));
        }
        let Cx {
            events,
            mut change,
            effects,
            new_blobs,
            dead_blobs,
            who,
            ..
        } = cx;
        Meter::stop(self.store.conn_mut());
        let conn = self.store.conn();
        if let Some(info) = change.as_mut() {
            let mut seen = std::collections::HashSet::new();
            info.effects = effects
                .into_iter()
                .filter(|e| seen.insert(e.clone()))
                .collect();
        }
        // SQLite rolls a transaction back by itself after some errors; then
        // nothing may be committed or logged.
        let lost = conn.is_autocommit();

        let outcome = match result {
            Ok(_) if lost => {
                self.drop_blobs(&new_blobs);
                ApiError::internal().into_response()
            }
            Ok(response) => {
                let mut response = response;
                if response.body_len() as u64 > budget.bytes {
                    let _ = conn.execute_batch("ROLLBACK");
                    self.drop_blobs(&new_blobs);
                    return exceeded("bytes").into_response();
                }
                if logged {
                    let _ = conn.execute_batch("RELEASE op");
                }
                // A handler that refuses with a body of its own (an import)
                // still logs its reason.
                let reason = (response.status >= 400).then(|| {
                    let b = response.json_body();
                    b["error"]
                        .as_str()
                        .or(b["message"].as_str())
                        .unwrap_or("")
                        .to_owned()
                });
                let seq = match (&request_id, &change) {
                    (Some(id), Some(info)) => {
                        match crate::changes::record(
                            conn,
                            id,
                            &who.actor(&self.catalog),
                            route.op(),
                            info,
                            response.status,
                            reason.as_deref(),
                            &time::rfc3339(now_ms),
                        ) {
                            Ok(seq) => Some(seq),
                            Err(e) => {
                                let _ = conn.execute_batch("ROLLBACK");
                                self.drop_blobs(&new_blobs);
                                return e.into_response();
                            }
                        }
                    }
                    _ => None,
                };
                // Webhook calls the change owes, kept with it.
                if response.status < 400
                    && let Err(e) = webhooks::queue(conn, &mut *self.host, &events, now_ms)
                {
                    let _ = conn.execute_batch("ROLLBACK");
                    self.drop_blobs(&new_blobs);
                    return ApiError::from(e).into_response();
                }
                if let Err(e) = conn.execute_batch("COMMIT") {
                    let _ = conn.execute_batch("ROLLBACK");
                    self.drop_blobs(&new_blobs);
                    return ApiError::from(e).into_response();
                }
                if let Some(seq) = seq {
                    self.last_seq = seq;
                    response
                        .headers
                        .push(("Moul-Change".into(), seq.to_string()));
                }
                if let Some(id) = &request_id {
                    response
                        .headers
                        .push(("Moul-Request-Id".into(), id.clone()));
                }
                self.drop_blobs(&dead_blobs);
                self.after_commit(events, now_ms);
                response
            }
            Err(err) if err.status < 500 && err.extra.get("budget").is_none() && !lost => {
                if logged {
                    let _ = self
                        .store
                        .conn()
                        .execute_batch("ROLLBACK TO op; RELEASE op");
                }
                self.drop_blobs(&new_blobs);
                let conn = self.store.conn();
                if logged && let Some(id) = &request_id {
                    let info = ChangeInfo {
                        collection: route
                            .collection()
                            .and_then(|n| self.catalog.get(n))
                            .map(|c| c.id.clone()),
                        record: route.record().map(str::to_owned),
                        ..ChangeInfo::default()
                    };
                    if let Err(e) = crate::changes::record(
                        conn,
                        id,
                        &who.actor(&self.catalog),
                        route.op(),
                        &info,
                        err.status,
                        Some(&err.message),
                        &time::rfc3339(now_ms),
                    ) {
                        let _ = conn.execute_batch("ROLLBACK");
                        return e.into_response();
                    }
                }
                // Handlers outside the change log (sign-in, codes) may have
                // written on purpose before refusing, as the Go engine does.
                if conn.execute_batch("COMMIT").is_err() {
                    let _ = conn.execute_batch("ROLLBACK");
                }
                let mut r = err.into_response();
                if let Some(id) = &request_id {
                    r.headers.push(("Moul-Request-Id".into(), id.clone()));
                }
                r
            }
            Err(err) => {
                if !lost {
                    let _ = self.store.conn().execute_batch("ROLLBACK");
                }
                self.drop_blobs(&new_blobs);
                err.into_response()
            }
        };

        if outcome.body_len() as u64 > budget.bytes {
            return exceeded("bytes").into_response();
        }
        outcome
    }

    /// Announces committed record changes to live subscriptions, after
    /// ending the subscriptions whose credentials no longer hold.
    fn after_commit(&mut self, events: Vec<RecordEvent>, now_ms: i64) {
        if events.is_empty() || self.catalog.refresh(self.store.conn()).is_err() {
            return;
        }
        self.recheck_subscriptions(now_ms);
        let signer = files::Signer {
            key: &self.key,
            ttl_ms: self.config.file_url_ttl_ms,
            now_ms,
        };
        self.realtime.publish_at(
            self.store.conn(),
            &self.catalog,
            &self.guard,
            &signer,
            events,
            Some(now_ms),
        );
    }

    /// Ends live subscriptions whose token was revoked or expired, whose
    /// admin key no longer matches, or whose user was deleted.
    fn recheck_subscriptions(&mut self, now_ms: i64) {
        let conn = self.store.conn();
        let (config, key, catalog) = (&self.config, &self.key, &self.catalog);
        self.realtime.retain(|spec, who| {
            if spec.owner {
                return None;
            }
            let mut req = Request::new("GET", "/");
            req.headers = spec.credentials.clone();
            let now = auth::identify(conn, config, key, &req, now_ms).ok();
            if now.as_ref() != Some(who) {
                return Some("credentials no longer valid".into());
            }
            if let Who::User(u) = who {
                let exists = catalog
                    .get(&u.collection)
                    .is_some_and(|c| records::load(conn, c, &u.id).ok().flatten().is_some());
                if !exists {
                    return Some("user deleted".into());
                }
            }
            None
        });
    }

    fn drop_blobs(&mut self, hashes: &[String]) {
        for h in hashes {
            let still_used: bool = self
                .store
                .conn()
                .query_row(
                    "SELECT 1 FROM main._files WHERE hash = ?1 LIMIT 1",
                    [h],
                    |_| Ok(()),
                )
                .optional()
                .ok()
                .flatten()
                .is_some();
            if !still_used {
                let _ = self.blobs.delete(self.store.conn(), h);
            }
        }
    }

    fn next_request_id(&mut self) -> ApiResult<String> {
        let conn = self.store.conn();
        let mut n: u64 = device_value(conn, "counter")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // Skip any id already in the log, however it got there.
        let id = loop {
            n += 1;
            let id = ids::request_id(&self.device, n);
            if crate::changes::find(conn, &id)?.is_none() {
                break id;
            }
        };
        set_device_value(conn, "counter", &n.to_string())?;
        Ok(id)
    }

    /// Answers a write whose id is already in the change log, without
    /// applying it again. The id must have been used by the same actor for
    /// the same operation on the same target; anything else is a conflict.
    fn replay(&mut self, route: &Route, req: &Request, who: Who, done: Change) -> Response {
        let _ = self.catalog.refresh(self.store.conn());
        // The target is judged by the logged collection id: a collection
        // renamed or deleted since still matches the request that did it.
        let by_path = route
            .collection()
            .and_then(|n| self.catalog.get(n))
            .cloned();
        let logged = done
            .collection
            .as_deref()
            .and_then(|id| self.catalog.by_id(id))
            .cloned();
        let same_target = match (&done.collection, &by_path) {
            (Some(id), Some(c)) => *id == c.id,
            (None, None) => true,
            (Some(_), None) => {
                done.applied()
                    && (logged.is_some()
                        || matches!(
                            route,
                            Route::CreateCollection
                                | Route::ApplySchema
                                | Route::DeleteCollection(_)
                                | Route::UpdateCollection(_)
                        ))
            }
            (None, Some(_)) => !done.applied(),
        } && route
            .record()
            .is_none_or(|r| done.record.as_deref() == Some(r));
        if done.actor != who.actor(&self.catalog) || done.op != route.op() || !same_target {
            return ApiError::new(
                409,
                format!(
                    "Request id {} was already used for another request",
                    done.request
                ),
            )
            .into_response();
        }
        let stub = || {
            let mut body = json!({"request": done.request, "replayed": true, "seq": done.seq});
            if let Some(r) = &done.record {
                body["id"] = json!(r);
            }
            Response::json(done.status, body)
        };
        let mut response = if done.status >= 400 {
            ApiError::new(done.status, done.error.clone().unwrap_or_default()).into_response()
        } else {
            match route {
                Route::CreateRecord(_) | Route::UpdateRecord(..) if done.record.is_some() => {
                    // Read the record as the sender sees it now, with their
                    // own rights. If they may no longer see it, or it is
                    // gone, the answer says only that the write was applied.
                    let name = logged.as_ref().or(by_path.as_ref()).map(|c| c.name.clone());
                    let id = done.record.clone().unwrap_or_default();
                    let mut get = req.clone();
                    get.method = "GET".into();
                    get.body = crate::api::Body::None;
                    get.id = None;
                    let r = match name {
                        Some(name) => self.serve(&Route::GetRecord(name, id), &get),
                        None => ApiError::not_found("Not found").into_response(),
                    };
                    if r.status == 200 {
                        Response {
                            status: done.status,
                            ..r
                        }
                    } else {
                        stub()
                    }
                }
                Route::CreateCollection | Route::UpdateCollection(_) => match &logged {
                    Some(c) => Response::json(done.status, c.to_json()),
                    None => stub(),
                },
                Route::Upload => {
                    match files::info_by_id(self.store.conn(), done.record.as_deref().unwrap_or(""))
                    {
                        Some(info) => Response::json(done.status, json!([info])),
                        None => stub(),
                    }
                }
                _ if done.status == 204 => Response::empty(204),
                _ => stub(),
            }
        };
        response
            .headers
            .push(("Moul-Request-Id".into(), done.request.clone()));
        response
            .headers
            .push(("Moul-Change".into(), done.seq.to_string()));
        response
            .headers
            .push(("Moul-Replayed".into(), "true".into()));
        response
    }
}

fn dispatch(cx: &mut Cx<'_>, route: &Route) -> ApiResult {
    match route {
        Route::ListCollections => collections::list(cx),
        Route::CreateCollection => collections::create(cx),
        Route::GetCollection(n) => collections::get(cx, n),
        Route::UpdateCollection(n) => collections::update(cx, n),
        Route::DeleteCollection(n) => collections::delete(cx, n),
        Route::GetEmailTemplates(n) => collections::get_templates(cx, n),
        Route::PutEmailTemplates(n) => collections::put_templates(cx, n),
        Route::Export(n) => dataio::export(cx, n),
        Route::Import(n) => dataio::import(cx, n),
        Route::ListRecords(n) => records::list(cx, n),
        Route::CreateRecord(n) => records::create(cx, n),
        Route::GetRecord(n, id) => records::view(cx, n, id),
        Route::UpdateRecord(n, id) => records::update(cx, n, id),
        Route::DeleteRecord(n, id) => records::delete(cx, n, id),
        Route::AuthWithPassword(n) => auth::with_password(cx, n),
        Route::RequestPasswordReset(n) => auth::request_password_reset(cx, n),
        Route::ConfirmPasswordReset(n) => auth::confirm_password_reset(cx, n),
        Route::Refresh(n) => auth::refresh(cx, n),
        Route::Logout(_) => auth::logout(cx),
        Route::RequestOtp(n) => auth::request_otp(cx, n),
        Route::AuthWithOtp(n) => auth::with_otp(cx, n),
        Route::AuthMethods(n) => oauth::methods(cx, n),
        Route::OAuthAuthorize(n, p) => oauth::authorize(cx, n, p),
        Route::TestEmailTemplate(n) => auth::test_template(cx, n),
        Route::SetupStatus => admin::status(cx),
        Route::Setup => admin::setup(cx),
        Route::AdminLogin => admin::login(cx),
        Route::RootAccount => admin::account(cx),
        Route::UpdateRootAccount => admin::update_account(cx),
        Route::UpdateRootPassword => admin::update_password(cx),
        Route::ReloadSettings => settings::reload_endpoint(cx),
        Route::GetSettings => settings::get_endpoint(cx),
        Route::UpdateSettings => settings::patch_endpoint(cx),
        Route::ListFlags => flags::list(cx),
        Route::CreateFlag => flags::create(cx),
        Route::GetFlag(k) => flags::get(cx, k),
        Route::UpdateFlag(k) => flags::update(cx, k),
        Route::DeleteFlag(k) => flags::delete(cx, k),
        Route::EvalFlag(k) => flags::eval(cx, k),
        Route::ListJobs => workers::list(cx),
        Route::CreateJob => workers::create(cx),
        Route::GetJob(id) => workers::get(cx, id),
        Route::UpdateJob(id) => workers::update(cx, id),
        Route::DeleteJob(id) => workers::delete(cx, id),
        Route::RetryJobs => workers::retry(cx),
        Route::ListWebhooks(n) => webhooks::list(cx, n),
        Route::CreateWebhook(n) => webhooks::create(cx, n),
        Route::GetWebhook(n, id) => webhooks::get(cx, n, id),
        Route::UpdateWebhook(n, id) => webhooks::update(cx, n, id),
        Route::DeleteWebhook(n, id) => webhooks::delete(cx, n, id),
        Route::TestWebhook(n) => webhooks::test(cx, n),
        Route::ExternalSignIn(n, profile) => auth::external(cx, n, profile),
        Route::Upload => files::upload(cx),
        Route::ListFiles => files::list(cx),
        Route::DeleteFile(t) => files::delete(cx, t),
        Route::Storage(p) => files::serve(cx, p),
        Route::RulesTest => rules::test_endpoint(cx),
        Route::Changes => crate::changes::endpoint(cx),
        Route::Inbox => auth::inbox_endpoint(cx),
        Route::GetSchema => definition::export_endpoint(cx),
        Route::ApplySchema => definition::apply_endpoint(cx),
        Route::Types => definition::types_endpoint(cx),
        Route::NotFound | Route::MethodNotAllowed => unreachable!("answered before dispatch"),
    }
}

/// Signs `data` with the engine's key for `purpose`, as the Go engine's
/// `DeriveKey` does.
pub(crate) fn derive_key(key: &[u8], purpose: &str) -> [u8; 32] {
    crypto::hmac_sha256(key, format!("moul:{purpose}").as_bytes())
}
