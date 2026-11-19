//! The hub's directory: who has an account, which devices may sync, which
//! databases each account keeps.
//!
//! It is a Moul database of its own. Accounts are the `accounts` auth
//! collection, so signing in by email code, its limits on guessing, and
//! the mail that carries the code are the engine's. Devices, databases,
//! join tickets and invites are tables in its writer-only store, which no
//! replica and no browser ever sees; device tokens are kept only as their
//! SHA-256. Times are milliseconds since the epoch.

use moul_server::db::Db;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// The auth collection that holds accounts.
pub const ACCOUNTS: &str = "accounts";

/// How long a join ticket lasts.
pub const JOIN_TTL_MS: i64 = 15 * 60 * 1000;

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS writer.hub_devices (
  id TEXT PRIMARY KEY,
  account TEXT NOT NULL,
  database TEXT,
  label TEXT NOT NULL DEFAULT '',
  tokenHash TEXT NOT NULL UNIQUE,
  createdAt INTEGER NOT NULL,
  lastSeenAt INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS writer.idx_hub_devices_account ON hub_devices (account);
CREATE TABLE IF NOT EXISTS writer.hub_databases (
  account TEXT NOT NULL,
  name TEXT NOT NULL,
  createdAt INTEGER NOT NULL,
  bytes INTEGER NOT NULL DEFAULT 0,
  pushedAt INTEGER,
  maintainedAt INTEGER,
  PRIMARY KEY (account, name)
);
CREATE TABLE IF NOT EXISTS writer.hub_joins (
  ticketHash TEXT PRIMARY KEY,
  account TEXT NOT NULL,
  database TEXT NOT NULL,
  expiresAt INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS writer.hub_invites (
  code TEXT PRIMARY KEY,
  uses INTEGER NOT NULL,
  createdAt INTEGER NOT NULL
);
";

/// An account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// Its id, which names its folder in the store.
    pub id: String,
    /// Its email.
    pub email: String,
}

impl Account {
    /// As answers show it.
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "email": self.email})
    }
}

/// A device that may sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// Its id.
    pub id: String,
    /// Its account's id.
    pub account: String,
    /// The one database it reaches, for a device that joined by link.
    pub database: Option<String>,
    /// A name for people.
    pub label: String,
    /// When it signed in.
    pub created_at: i64,
    /// When it was last seen, to the minute.
    pub last_seen_at: i64,
}

impl Device {
    /// As answers show it; `current` marks the asking device.
    pub fn to_json(&self, current: &str) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "database": self.database,
            "createdAt": self.created_at,
            "lastSeenAt": self.last_seen_at,
            "current": self.id == current,
        })
    }

    /// Whether this device may reach `database`.
    pub fn reaches(&self, database: &str) -> bool {
        self.database.as_deref().is_none_or(|d| d == database)
    }
}

/// A database an account keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Database {
    /// Its name.
    pub name: String,
    /// When it was made.
    pub created_at: i64,
    /// Bytes it holds, as last counted.
    pub bytes: i64,
}

fn now() -> i64 {
    moul_vfs::system_clock()
}

fn random(n: usize) -> String {
    moul_core::ids::random_chars(&mut moul_server::open::OsRandom, n)
}

fn hash(s: &str) -> String {
    moul_core::crypto::sha256_hex(s.as_bytes())
}

/// Makes the tables and the accounts collection if they are not there.
pub fn prepare(db: &mut Db) -> Result<(), String> {
    db.engine
        .store()
        .conn()
        .execute_batch(TABLES)
        .map_err(|e| e.to_string())?;
    let exists = db
        .engine
        .handle(moul_core::Request::get(&format!("/api/moul/{ACCOUNTS}")).owner())
        .status
        == 200;
    if !exists {
        let r = db.engine.handle(
            moul_core::Request::post(
                "/api/moul",
                json!({"name": ACCOUNTS, "type": "auth", "fields": []}),
            )
            .owner(),
        );
        if r.status >= 300 {
            return Err(format!(
                "cannot make the accounts collection: {}",
                r.message()
            ));
        }
    }
    Ok(())
}

/// The account with `email`.
pub fn account_by_email(db: &Db, email: &str) -> Option<Account> {
    db.engine
        .store()
        .conn()
        .query_row(
            &format!("SELECT id, email FROM main.\"{ACCOUNTS}\" WHERE email = ?1"),
            [email],
            |r| {
                Ok(Account {
                    id: r.get(0)?,
                    email: r.get(1)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
}

/// The account `id`.
pub fn account(db: &Db, id: &str) -> Option<Account> {
    db.engine
        .store()
        .conn()
        .query_row(
            &format!("SELECT id, email FROM main.\"{ACCOUNTS}\" WHERE id = ?1"),
            [id],
            |r| {
                Ok(Account {
                    id: r.get(0)?,
                    email: r.get(1)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
}

/// Every account, for the admin.
pub fn accounts(db: &Db) -> Vec<Value> {
    let conn = db.engine.store().conn();
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT a.id, a.email, a.createdAt,
           (SELECT count(*) FROM writer.hub_databases d WHERE d.account = a.id),
           (SELECT coalesce(sum(bytes), 0) FROM writer.hub_databases d WHERE d.account = a.id)
         FROM main.\"{ACCOUNTS}\" a ORDER BY a.createdAt"
    )) else {
        return Vec::new();
    };
    stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?, "email": r.get::<_, String>(1)?,
            "createdAt": r.get::<_, String>(2)?, "databases": r.get::<_, i64>(3)?, "bytes": r.get::<_, i64>(4)?,
        }))
    })
    .map(|it| it.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

fn device_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: r.get(0)?,
        account: r.get(1)?,
        database: r.get(2)?,
        label: r.get(3)?,
        created_at: r.get(4)?,
        last_seen_at: r.get(5)?,
    })
}

const DEVICE_COLUMNS: &str = "id, account, database, label, createdAt, lastSeenAt";

/// Signs a device in: a new token, of which only the hash is kept.
pub fn create_device(
    db: &Db,
    account: &str,
    database: Option<&str>,
    label: &str,
) -> Result<(String, Device), String> {
    let token = format!("mh_{}", random(40));
    let id = format!("dev_{}", random(12));
    let t = now();
    let label: String = label.trim().chars().take(80).collect();
    db.engine
        .store()
        .conn()
        .execute(
            "INSERT INTO writer.hub_devices (id, account, database, label, tokenHash, createdAt, lastSeenAt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, account, database, label, hash(&token), t],
        )
        .map_err(|e| e.to_string())?;
    Ok((
        token,
        Device {
            id,
            account: account.to_owned(),
            database: database.map(str::to_owned),
            label,
            created_at: t,
            last_seen_at: t,
        },
    ))
}

/// The device a token belongs to; marks it seen.
pub fn device_for_token(db: &Db, token: &str) -> Option<Device> {
    if !token.starts_with("mh_") {
        return None;
    }
    let conn = db.engine.store().conn();
    let mut d = conn
        .query_row(
            &format!("SELECT {DEVICE_COLUMNS} FROM writer.hub_devices WHERE tokenHash = ?1"),
            [hash(token)],
            device_row,
        )
        .optional()
        .ok()
        .flatten()?;
    let t = now();
    if t - d.last_seen_at > 60_000 {
        let _ = conn.execute(
            "UPDATE writer.hub_devices SET lastSeenAt = ?1 WHERE id = ?2",
            params![t, d.id],
        );
        d.last_seen_at = t;
    }
    Some(d)
}

/// An account's devices, newest first.
pub fn devices(db: &Db, account: &str) -> Vec<Device> {
    let conn = db.engine.store().conn();
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {DEVICE_COLUMNS} FROM writer.hub_devices WHERE account = ?1 ORDER BY createdAt DESC"
    )) else {
        return Vec::new();
    };
    stmt.query_map([account], device_row)
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// Revokes device `id` of `account`. Returns whether it existed.
pub fn delete_device(db: &Db, account: &str, id: &str) -> bool {
    db.engine
        .store()
        .conn()
        .execute(
            "DELETE FROM writer.hub_devices WHERE id = ?1 AND account = ?2",
            params![id, account],
        )
        .is_ok_and(|n| n > 0)
}

fn database_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Database> {
    Ok(Database {
        name: r.get(0)?,
        created_at: r.get(1)?,
        bytes: r.get(2)?,
    })
}

/// An account's databases, by name.
pub fn databases(db: &Db, account: &str) -> Vec<Database> {
    let conn = db.engine.store().conn();
    let Ok(mut stmt) = conn.prepare(
        "SELECT name, createdAt, bytes FROM writer.hub_databases WHERE account = ?1 ORDER BY name",
    ) else {
        return Vec::new();
    };
    stmt.query_map([account], database_row)
        .map(|it| it.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// One database.
pub fn database(db: &Db, account: &str, name: &str) -> Option<Database> {
    db.engine
        .store()
        .conn()
        .query_row(
            "SELECT name, createdAt, bytes FROM writer.hub_databases WHERE account = ?1 AND name = ?2",
            params![account, name],
            database_row,
        )
        .optional()
        .ok()
        .flatten()
}

/// Makes a database: `Ok(None)` if the account has one by that name.
pub fn create_database(db: &Db, account: &str, name: &str) -> Result<Option<Database>, String> {
    let t = now();
    let n = db
        .engine
        .store()
        .conn()
        .execute(
            "INSERT OR IGNORE INTO writer.hub_databases (account, name, createdAt) VALUES (?1, ?2, ?3)",
            params![account, name, t],
        )
        .map_err(|e| e.to_string())?;
    Ok((n > 0).then(|| Database {
        name: name.to_owned(),
        created_at: t,
        bytes: 0,
    }))
}

/// Forgets a database, its devices and its tickets.
pub fn delete_database(db: &Db, account: &str, name: &str) {
    let conn = db.engine.store().conn();
    let _ = conn.execute(
        "DELETE FROM writer.hub_databases WHERE account = ?1 AND name = ?2",
        params![account, name],
    );
    let _ = conn.execute(
        "DELETE FROM writer.hub_devices WHERE account = ?1 AND database = ?2",
        params![account, name],
    );
    let _ = conn.execute(
        "DELETE FROM writer.hub_joins WHERE account = ?1 AND database = ?2",
        params![account, name],
    );
}

/// The bytes an account's databases hold, and how many there are.
pub fn usage(db: &Db, account: &str) -> (i64, i64) {
    db.engine
        .store()
        .conn()
        .query_row(
            "SELECT count(*), coalesce(sum(bytes), 0) FROM writer.hub_databases WHERE account = ?1",
            [account],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0))
}

/// Adds `delta` to a database's bytes and notes a push.
pub fn add_bytes(db: &Db, account: &str, name: &str, delta: i64) {
    let _ = db.engine.store().conn().execute(
        "UPDATE writer.hub_databases SET bytes = max(0, bytes + ?1), pushedAt = ?2 WHERE account = ?3 AND name = ?4",
        params![delta, now(), account, name],
    );
}

/// Sets a database's bytes from a count, after maintenance.
pub fn maintained(db: &Db, account: &str, name: &str, bytes: i64) {
    let _ = db.engine.store().conn().execute(
        "UPDATE writer.hub_databases SET bytes = ?1, maintainedAt = ?2 WHERE account = ?3 AND name = ?4",
        params![bytes, now(), account, name],
    );
}

/// Databases pushed to since they were last maintained, or not maintained
/// for a day (their snapshot may be due), at most `limit`.
pub fn due_for_maintenance(db: &Db, every_ms: i64, limit: usize) -> Vec<(String, String)> {
    let t = now();
    let conn = db.engine.store().conn();
    let Ok(mut stmt) = conn.prepare(
        "SELECT account, name FROM writer.hub_databases
         WHERE (pushedAt IS NOT NULL AND (maintainedAt IS NULL OR pushedAt > maintainedAt) AND coalesce(maintainedAt, 0) < ?1)
            OR (maintainedAt IS NOT NULL AND maintainedAt < ?2)
         ORDER BY coalesce(maintainedAt, 0) LIMIT ?3",
    ) else {
        return Vec::new();
    };
    stmt.query_map(
        params![t - every_ms, t - 24 * 3600 * 1000, limit as i64],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map(|it| it.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// A ticket that lets one device join `database`.
pub fn create_join(db: &Db, account: &str, database: &str) -> Result<(String, i64), String> {
    let ticket = format!("mhj_{}", random(32));
    let expires = now() + JOIN_TTL_MS;
    let conn = db.engine.store().conn();
    let _ = conn.execute("DELETE FROM writer.hub_joins WHERE expiresAt < ?1", [now()]);
    conn.execute(
        "INSERT INTO writer.hub_joins (ticketHash, account, database, expiresAt) VALUES (?1, ?2, ?3, ?4)",
        params![hash(&ticket), account, database, expires],
    )
    .map_err(|e| e.to_string())?;
    Ok((ticket, expires))
}

/// Uses a ticket up: the account and database it was for.
pub fn take_join(db: &Db, ticket: &str) -> Option<(String, String)> {
    db.engine
        .store()
        .conn()
        .query_row(
            "DELETE FROM writer.hub_joins WHERE ticketHash = ?1 AND expiresAt > ?2 RETURNING account, database",
            params![hash(ticket), now()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()
}

/// Makes an invite code good for `uses` sign-ups.
pub fn create_invite(db: &Db, uses: i64) -> Result<String, String> {
    let code = random(10).to_ascii_uppercase();
    db.engine
        .store()
        .conn()
        .execute(
            "INSERT INTO writer.hub_invites (code, uses, createdAt) VALUES (?1, ?2, ?3)",
            params![code, uses.max(1), now()],
        )
        .map_err(|e| e.to_string())?;
    Ok(code)
}

/// Uses one sign-up of an invite code. Returns whether it was good.
pub fn take_invite(db: &Db, code: &str) -> bool {
    let conn = db.engine.store().conn();
    let n = conn
        .execute(
            "UPDATE writer.hub_invites SET uses = uses - 1 WHERE code = ?1 AND uses > 0",
            [code.trim().to_ascii_uppercase()],
        )
        .unwrap_or(0);
    n > 0
}
