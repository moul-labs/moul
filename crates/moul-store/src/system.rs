//! System tables of the three stores, their migrations, and converting a
//! database the Go engine made.
//!
//! Each store records its format in `PRAGMA user_version`. The shared store
//! also keeps a `_meta` table with the format and the oldest runtime that
//! may open it, so a device with an older engine refuses a newer file
//! instead of damaging it.
//!
//! | Store       | Table              | Holds                                                   |
//! | ----------- | ------------------ | ------------------------------------------------------- |
//! | shared      | `_moul`            | Collection definitions (the Go layout, plus field ids)  |
//! | shared      | `_moul_version`    | A counter of definition changes, kept by triggers       |
//! | shared      | `_meta`            | Format, oldest runtime, origin, a wipe still due        |
//! | shared      | `_changes`         | The change log: one row for each write request          |
//! | shared      | `_files`           | Uploaded files: content hash, size, type, owner         |
//! | shared      | `_file_refs`       | Which record field points at which file                 |
//! | writer-only | `_auth`            | Password hashes, one-time codes, reset tokens           |
//! | writer-only | `_keys`            | Signing keys                                            |
//! | writer-only | `_settings`        | Settings, some of them secret                           |
//! | writer-only | `_inbox`           | Mail the engine would send, kept locally                |
//! | writer-only | `_revoked_tokens`  | Tokens signed out before they expire                    |
//! | writer-only | `_rootUsers`       | Administrators of a native server                       |
//! | writer-only | `_webhook_secrets` | Secrets of webhooks kept from a Go database             |
//! | writer-only | `_worker_args`     | Rendered mail of Go mail jobs in worker collections     |
//! | writer-only | `_outbox`          | Webhook calls a write queued, until they are delivered  |
//! | writer-only | `_workers`         | Background jobs (the Go engine's table)                 |
//! | writer-only | `_requests`        | The request log of a native server                      |
//! | writer-only | `_visits`          | Visits a native server counted                          |
//! | writer-only | `_oauth_clients`   | MCP clients registered with a server's OAuth endpoints  |
//! | writer-only | `_oauth_refresh`   | Their refresh tokens, hashed                            |
//! | shared      | `_feature_flags`   | Feature flags and their targeting                       |
//! | device      | `_device`          | This device's id and request counter                    |
//! | device      | `_queue`           | Requests not yet confirmed by the writer (Phase 9)      |
//! | device      | `_blobs`           | Uploaded file bytes by SHA-256, when kept in SQLite     |
//!
//! Tables added after a format was released are created, and columns added,
//! each time a store opens ([`ADDITIONS`]): an older engine opens such a
//! store and ignores what it does not know, so they need no new format.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::catalog;
use crate::schema::{CollectionKind, quote};
use crate::{Error, Result, Store, StoreKind};

/// The format of the stores this engine writes.
pub const FORMAT: i64 = 1;

/// The oldest runtime that can open a database of [`FORMAT`]. Recorded in
/// `_meta` so older engines can refuse newer files.
pub const MIN_RUNTIME: &str = "0.1.0";

/// Columns of a Go auth table that hold credentials. They move to the
/// writer-only store's `_auth` table when a Go database converts.
pub const GO_SECRET_COLUMNS: [&str; 9] = [
    "passwordHash",
    "tokenKey",
    "otpCode",
    "otpExpiresAt",
    "otpAttempts",
    "passkeys",
    "resetToken",
    "resetTokenExpiresAt",
    "oauthProviders",
];

/// Go system tables that hold secrets or personal logs. They move to the
/// writer-only store when a Go database converts. `_workers` holds queued
/// mail, rendered with one-time codes and reset links.
const GO_WRITER_TABLES: [&str; 7] = [
    "_settings",
    "_rootUsers",
    "_revoked_tokens",
    "_requests",
    "_visits",
    "_certmagic",
    "_workers",
];

/// The Go engine's job that sends a mail. When the engine queued it, its
/// arguments hold the rendered mail.
const GO_MAIL_WORKER: &str = "SendEmail";

/// Go system tables with no use here, dropped when a Go database converts.
const GO_DROPPED_TABLES: [&str; 1] = ["_realtime_tickets"];

const SHARED_V1: &str = "
CREATE TABLE IF NOT EXISTS main._moul (
  id TEXT PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  type TEXT NOT NULL,
  fields TEXT NOT NULL,
  rules TEXT NOT NULL,
  createdAt TEXT NOT NULL,
  updatedAt TEXT NOT NULL,
  email_templates TEXT,
  webhooks TEXT
);
CREATE TABLE IF NOT EXISTS main._moul_version (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  version INTEGER NOT NULL
);
INSERT OR IGNORE INTO main._moul_version (id, version) VALUES (1, 0);
CREATE TRIGGER IF NOT EXISTS main._moul_version_after_insert AFTER INSERT ON _moul
BEGIN UPDATE _moul_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS main._moul_version_after_update AFTER UPDATE ON _moul
BEGIN UPDATE _moul_version SET version = version + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS main._moul_version_after_delete AFTER DELETE ON _moul
BEGIN UPDATE _moul_version SET version = version + 1 WHERE id = 1; END;
CREATE TABLE IF NOT EXISTS main._meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS main._changes (
  seq INTEGER PRIMARY KEY,
  request TEXT NOT NULL UNIQUE,
  actor TEXT NOT NULL,
  op TEXT NOT NULL,
  collection TEXT,
  record TEXT,
  fields TEXT,
  status INTEGER NOT NULL,
  error TEXT,
  at TEXT NOT NULL,
  effects TEXT
);
CREATE INDEX IF NOT EXISTS main.idx_changes_record ON _changes (collection, record);
CREATE TABLE IF NOT EXISTS main._files (
  id TEXT PRIMARY KEY,
  ownerMoul TEXT NOT NULL DEFAULT '',
  ownerId TEXT NOT NULL DEFAULT '',
  createdAt TEXT NOT NULL,
  hash TEXT NOT NULL DEFAULT '',
  size INTEGER NOT NULL DEFAULT 0,
  type TEXT NOT NULL DEFAULT '',
  name TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS main._file_refs (
  fileId TEXT NOT NULL,
  moul TEXT NOT NULL,
  recordId TEXT NOT NULL,
  field TEXT NOT NULL,
  PRIMARY KEY (fileId, moul, recordId, field)
);
CREATE INDEX IF NOT EXISTS main.idx_file_refs_record ON _file_refs (moul, recordId);
";

const WRITER_V1: &str = "
CREATE TABLE IF NOT EXISTS writer._auth (
  collection TEXT NOT NULL,
  record TEXT NOT NULL,
  passwordHash TEXT,
  tokenKey TEXT,
  otpCode TEXT,
  otpExpiresAt TEXT,
  otpAttempts INTEGER NOT NULL DEFAULT 0,
  passkeys TEXT,
  resetToken TEXT,
  resetTokenExpiresAt TEXT,
  oauthProviders TEXT,
  PRIMARY KEY (collection, record)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS writer._keys (
  name TEXT PRIMARY KEY,
  value BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS writer._settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS writer._inbox (
  id TEXT PRIMARY KEY,
  collection TEXT NOT NULL,
  recipient TEXT NOT NULL,
  kind TEXT NOT NULL,
  subject TEXT NOT NULL,
  body TEXT NOT NULL,
  createdAt TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS writer.idx_inbox_recipient ON _inbox (recipient, createdAt);
CREATE TABLE IF NOT EXISTS writer._revoked_tokens (
  token_hash TEXT PRIMARY KEY,
  expires_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS writer.idx_revoked_tokens_expires ON _revoked_tokens (expires_at);
CREATE TABLE IF NOT EXISTS writer._rootUsers (
  id TEXT PRIMARY KEY,
  username TEXT UNIQUE NOT NULL,
  email TEXT UNIQUE NOT NULL,
  name TEXT NOT NULL DEFAULT '',
  passwordHash TEXT NOT NULL,
  createdAt TEXT NOT NULL,
  updatedAt TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS writer._webhook_secrets (
  collection TEXT NOT NULL,
  webhook TEXT NOT NULL,
  secret TEXT NOT NULL,
  PRIMARY KEY (collection, webhook)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS writer._worker_args (
  collection TEXT NOT NULL,
  job TEXT NOT NULL,
  args TEXT NOT NULL,
  PRIMARY KEY (collection, job)
);
";

const DEVICE_V1: &str = "
CREATE TABLE IF NOT EXISTS device._device (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS device._queue (
  request TEXT PRIMARY KEY,
  body TEXT NOT NULL,
  createdAt TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS device._blobs (
  hash TEXT PRIMARY KEY,
  data BLOB NOT NULL
) WITHOUT ROWID;
";

/// Tables and columns added to format 1 since it was first written, applied
/// with `IF NOT EXISTS` each time the stores open. Older engines ignore them.
/// The Go engine's `_workers`, `_requests` and `_visits` keep its layout, so
/// a converted Go database fills them as they are.
const WRITER_ADDITIONS: &str = "
CREATE TABLE IF NOT EXISTS writer._outbox (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  collection TEXT NOT NULL DEFAULT '',
  target TEXT NOT NULL DEFAULT '',
  url TEXT NOT NULL,
  event TEXT NOT NULL DEFAULT '',
  payload TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'pending',
  nextAt TEXT NOT NULL,
  lastError TEXT,
  createdAt TEXT NOT NULL,
  sentAt TEXT
);
CREATE INDEX IF NOT EXISTS writer.idx_outbox_due ON _outbox (status, nextAt);
CREATE TABLE IF NOT EXISTS writer._workers (
  id TEXT PRIMARY KEY,
  createdAt TEXT,
  updatedAt TEXT,
  state TEXT DEFAULT 'available',
  queue TEXT DEFAULT 'default',
  worker TEXT NOT NULL,
  args TEXT DEFAULT '{}',
  meta TEXT DEFAULT '{}',
  tags TEXT DEFAULT '[]',
  errors TEXT DEFAULT '[]',
  attempt INTEGER DEFAULT 0,
  max_attempts INTEGER DEFAULT 20,
  priority INTEGER DEFAULT 0,
  inserted_at TEXT NOT NULL,
  scheduled_at TEXT NOT NULL,
  attempted_at TEXT,
  attempted_by TEXT,
  cancelled_at TEXT,
  completed_at TEXT,
  discarded_at TEXT
);
CREATE INDEX IF NOT EXISTS writer.idx__workers_job_processing ON _workers (state, queue, priority, scheduled_at, id);
CREATE TABLE IF NOT EXISTS writer._requests (
  id TEXT PRIMARY KEY,
  visit_id TEXT,
  method TEXT NOT NULL,
  path TEXT NOT NULL,
  status_code INTEGER NOT NULL,
  response_time_ms INTEGER NOT NULL,
  createdAt TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS writer.idx_requests_created ON _requests (createdAt);
CREATE TABLE IF NOT EXISTS writer._visits (
  id TEXT PRIMARY KEY,
  visitor_token TEXT NOT NULL,
  user_id TEXT,
  ip TEXT,
  user_agent TEXT,
  referrer TEXT,
  referring_domain TEXT,
  landing_page TEXT,
  browser TEXT,
  os TEXT,
  device_type TEXT,
  country TEXT,
  region TEXT,
  city TEXT,
  utm_source TEXT,
  utm_medium TEXT,
  utm_term TEXT,
  utm_content TEXT,
  utm_campaign TEXT,
  started_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS writer.idx_visits_started ON _visits (started_at);
CREATE TABLE IF NOT EXISTS writer._oauth_clients (
  id TEXT PRIMARY KEY,
  secretHash TEXT,
  name TEXT NOT NULL,
  redirectUris TEXT NOT NULL,
  grantTypes TEXT NOT NULL,
  responseTypes TEXT NOT NULL,
  authMethod TEXT NOT NULL,
  createdAt TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS writer._oauth_refresh (
  tokenHash TEXT PRIMARY KEY,
  client TEXT NOT NULL,
  rootUser TEXT NOT NULL,
  scope TEXT NOT NULL,
  expiresAt TEXT NOT NULL
);
";

/// Columns added to writer-only tables of format 1: the delivery state of
/// mail, which a native server sends and a tab keeps.
const WRITER_ADDED_COLUMNS: [(&str, &str, &str); 5] = [
    ("_inbox", "status", "TEXT"),
    ("_inbox", "attempts", "INTEGER NOT NULL DEFAULT 0"),
    ("_inbox", "nextAt", "TEXT"),
    ("_inbox", "sentAt", "TEXT"),
    ("_inbox", "error", "TEXT"),
];

/// Shared tables added to format 1. Only the writer creates them; a
/// follower gets them with the writer's pages.
const SHARED_ADDITIONS: &str = "
CREATE TABLE IF NOT EXISTS main._feature_flags (
  id TEXT PRIMARY KEY,
  key TEXT UNIQUE NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  enabled INTEGER NOT NULL DEFAULT 1,
  default_value TEXT NOT NULL DEFAULT 'false',
  gates TEXT NOT NULL DEFAULT '{}',
  createdAt TEXT NOT NULL,
  updatedAt TEXT NOT NULL
);
";

/// Everything [`Store::migrate`] adds to format 1 stores each time they
/// open: the writer-only tables and columns, then the shared tables.
pub const ADDITIONS: [&str; 2] = [WRITER_ADDITIONS, SHARED_ADDITIONS];

fn add_writer_additions(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(WRITER_ADDITIONS)?;
    for (table, column, decl) in WRITER_ADDED_COLUMNS {
        if !column_names(tx, "writer", table)?
            .iter()
            .any(|c| c.eq_ignore_ascii_case(column))
        {
            tx.execute_batch(&format!(
                "ALTER TABLE writer.{} ADD COLUMN {column} {decl}",
                quote(table)
            ))?;
        }
    }
    Ok(())
}

/// What [`Store::migrate`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Migrated {
    /// The shared store was empty and got its system tables.
    pub created: bool,
    /// The shared store was a Go engine database and was converted.
    pub converted_from_go: Option<GoConversion>,
}

/// What converting a Go database changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoConversion {
    /// Collections whose fields got ids.
    pub collections: usize,
    /// Auth records whose credentials moved to the writer-only store.
    pub credentials_moved: usize,
    /// Tables moved to the writer-only store.
    pub tables_moved: Vec<String>,
    /// Tables dropped.
    pub tables_dropped: Vec<String>,
    /// The engine's own mail jobs in worker collections whose arguments,
    /// the rendered mail, moved to `writer._worker_args`.
    pub mail_jobs_moved: usize,
    /// Indexes dropped because they used a credential column.
    pub indexes_dropped: Vec<String>,
}

fn user_version(conn: &Connection, schema: &str) -> Result<i64> {
    Ok(conn.query_row(&format!("PRAGMA {schema}.user_version"), [], |r| r.get(0))?)
}

fn has_table(conn: &Connection, schema: &str, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            &format!("SELECT 1 FROM {schema}.sqlite_schema WHERE type = 'table' AND name = ?1"),
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn column_names(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA {schema}.table_info({})", quote(table)))?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names)
}

fn too_new(kind: StoreKind, version: i64) -> Error {
    Error::TooNew(format!(
        "the {kind} has format {version}; this engine reads format {FORMAT} and older. Update Moul to open it"
    ))
}

fn migrate_local_stores(tx: &Transaction<'_>) -> Result<()> {
    for (kind, sql) in [
        (StoreKind::WriterOnly, WRITER_V1),
        (StoreKind::Device, DEVICE_V1),
    ] {
        let schema = kind.schema();
        let v = user_version(tx, schema)?;
        if v > FORMAT {
            return Err(too_new(kind, v));
        }
        if v < 1 {
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA {schema}.user_version = 1"))?;
        }
    }
    add_writer_additions(tx)
}

impl Store {
    /// Creates or upgrades the system tables of all three stores, and
    /// converts a Go engine database in the shared store.
    ///
    /// Converting needs new field ids, so `new_field_id` is called once for
    /// each field of a Go collection; it is never called otherwise. The
    /// whole migration is one transaction. Converting also wipes the
    /// space that held moved credentials (`secure_delete`, then `VACUUM`),
    /// so the shared file carries no trace of them. The conversion records
    /// that this wipe is due, so if it fails, the next call does it.
    ///
    /// Only the writer migrates the shared store; a follower calls
    /// [`migrate_local`](Self::migrate_local).
    pub fn migrate(&mut self, new_field_id: impl FnMut() -> String) -> Result<Migrated> {
        self.check_open()?;
        let v = user_version(self.conn(), "main")?;
        if v > FORMAT {
            return Err(too_new(StoreKind::Shared, v));
        }
        if v == FORMAT {
            self.write(|tx| -> Result<()> {
                migrate_local_stores(tx)?;
                Ok(tx.execute_batch(SHARED_ADDITIONS)?)
            })?;
            // Format 1 is unreleased: a development store made before the
            // change log carried effects gets the column now.
            if !column_names(self.conn(), "main", "_changes")?
                .iter()
                .any(|c| c == "effects")
            {
                self.write(|tx| {
                    tx.execute_batch("ALTER TABLE main._changes ADD COLUMN effects TEXT")
                })?;
            }
            if scrub_pending(self.conn())? {
                self.scrub()?;
            }
            return Ok(Migrated::default());
        }
        let is_go = v == 0 && has_table(self.conn(), "main", "_moul")?;
        if is_go {
            self.conn()
                .execute_batch("PRAGMA main.secure_delete = ON")?;
        }
        let migrated = self.write(|tx| -> Result<Migrated> {
            migrate_local_stores(tx)?;
            let mut out = Migrated::default();
            if is_go {
                out.converted_from_go = Some(convert_go(tx, new_field_id)?);
                tx.execute(
                    "INSERT OR REPLACE INTO main._meta (key, value) VALUES ('scrub', 'pending')",
                    [],
                )?;
            } else {
                out.created = true;
                tx.execute_batch(SHARED_V1)?;
            }
            tx.execute_batch(SHARED_ADDITIONS)?;
            tx.execute(
                "INSERT OR REPLACE INTO main._meta (key, value) VALUES ('format', ?1), ('min_runtime', ?2)",
                params![FORMAT.to_string(), MIN_RUNTIME],
            )?;
            tx.execute_batch(&format!("PRAGMA main.user_version = {FORMAT}"))?;
            Ok(out)
        });
        if is_go && migrated.is_err() {
            let _ = self.conn().execute_batch("PRAGMA main.secure_delete = OFF");
        }
        let migrated = migrated?;
        if is_go {
            self.scrub()?;
        }
        Ok(migrated)
    }

    /// Rewrites the shared store with `VACUUM` and `secure_delete`, so no
    /// free space keeps what a conversion moved out, then clears the mark
    /// that the wipe is due.
    fn scrub(&mut self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("PRAGMA main.secure_delete = ON")?;
        let vacuumed = conn.execute_batch("VACUUM main");
        let cleared = vacuumed.and_then(|()| {
            conn.execute("DELETE FROM main._meta WHERE key = 'scrub'", [])
                .map(drop)
        });
        let reset = conn.execute_batch("PRAGMA main.secure_delete = OFF");
        cleared?;
        reset?;
        Ok(())
    }

    /// Creates or upgrades the writer-only and device stores only, leaving
    /// the shared store untouched. A follower's shared store gets its
    /// system tables from the writer, page by page. Fails if the shared
    /// store has a newer format than this engine reads.
    pub fn migrate_local(&mut self) -> Result<()> {
        self.check_open()?;
        let v = user_version(self.conn(), "main")?;
        if v > FORMAT {
            return Err(too_new(StoreKind::Shared, v));
        }
        self.write(migrate_local_stores)
    }

    /// The format of the shared store: 0 before [`migrate`](Self::migrate).
    pub fn format(&self) -> Result<i64> {
        self.check_open()?;
        user_version(self.conn(), "main")
    }
}

/// Whether a conversion's wipe of the shared store is still due.
fn scrub_pending(conn: &Connection) -> Result<bool> {
    if !has_table(conn, "main", "_meta")? {
        return Ok(false);
    }
    Ok(conn
        .query_row("SELECT 1 FROM main._meta WHERE key = 'scrub'", [], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

/// Converts the Go engine layout in `main` into the three-store layout.
fn convert_go(
    tx: &Transaction<'_>,
    mut new_field_id: impl FnMut() -> String,
) -> Result<GoConversion> {
    let mut out = GoConversion::default();

    // Older Go databases lack these columns; the Go engine added them on
    // start-up the same way.
    let moul_cols = column_names(tx, "main", "_moul")?;
    for col in ["email_templates", "webhooks"] {
        if !moul_cols.iter().any(|c| c.eq_ignore_ascii_case(col)) {
            tx.execute_batch(&format!("ALTER TABLE main._moul ADD COLUMN {col} TEXT"))?;
        }
    }
    if has_table(tx, "main", "_files")? {
        let cols = column_names(tx, "main", "_files")?;
        for (col, decl) in [
            ("hash", "TEXT NOT NULL DEFAULT ''"),
            ("size", "INTEGER NOT NULL DEFAULT 0"),
            ("type", "TEXT NOT NULL DEFAULT ''"),
            ("name", "TEXT NOT NULL DEFAULT ''"),
        ] {
            if !cols.iter().any(|c| c.eq_ignore_ascii_case(col)) {
                tx.execute_batch(&format!("ALTER TABLE main._files ADD COLUMN {col} {decl}"))?;
            }
        }
    }
    tx.execute_batch(SHARED_V1)?;

    // Settings and other secret tables move to the writer-only store. Where
    // this engine has its own table, rows are copied into it by column.
    for table in GO_WRITER_TABLES {
        if !has_table(tx, "main", table)? {
            continue;
        }
        if has_table(tx, "writer", table)? {
            let theirs = column_names(tx, "main", table)?;
            let ours: HashSet<String> = column_names(tx, "writer", table)?
                .into_iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            let shared: Vec<String> = theirs
                .into_iter()
                .filter(|c| ours.contains(&c.to_ascii_lowercase()))
                .map(|c| quote(&c))
                .collect();
            let cols = shared.join(", ");
            tx.execute_batch(&format!(
                "INSERT OR REPLACE INTO writer.{t} ({cols}) SELECT {cols} FROM main.{t}",
                t = quote(table)
            ))?;
        } else {
            tx.execute_batch(&format!(
                "CREATE TABLE writer.{t} AS SELECT * FROM main.{t}",
                t = quote(table)
            ))?;
        }
        tx.execute_batch(&format!("DROP TABLE main.{}", quote(table)))?;
        out.tables_moved.push(table.to_owned());
    }
    for table in GO_DROPPED_TABLES {
        if has_table(tx, "main", table)? {
            tx.execute_batch(&format!("DROP TABLE main.{}", quote(table)))?;
            out.tables_dropped.push(table.to_owned());
        }
    }

    for mut c in catalog::load_all(tx)? {
        // Field ids, so that later renames keep the column.
        for f in &mut c.fields {
            if f.id.is_empty() {
                f.id = new_field_id();
            }
        }
        // Saving moves webhook secrets to the writer-only store.
        catalog::save(tx, &c)?;
        out.collections += 1;

        if !catalog::table_exists(tx, &c.name)? {
            continue;
        }
        match c.kind {
            CollectionKind::Auth => {
                let moved = move_credentials(tx, &c.id, &c.name)?;
                out.credentials_moved += moved.records;
                out.indexes_dropped.extend(moved.indexes_dropped);
            }
            CollectionKind::Worker => {
                out.mail_jobs_moved += move_mail_jobs(tx, &c.id, &c.name)?;
            }
            _ => {}
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO main._meta (key, value) VALUES ('converted_from', 'go')",
        [],
    )?;
    Ok(out)
}

/// Moves the arguments of the engine's own mail jobs in the Go worker
/// collection `table` to `writer._worker_args`, and leaves `{}` in their
/// place. Returns the jobs moved.
///
/// An engine's mail job is a `SendEmail` job whose arguments carry a
/// rendered `body`, with one-time codes and reset links. The Go engine
/// queued those in a worker collection only before it had `_workers`.
/// Every other job, such as one a user queued without a `body`, keeps its
/// arguments.
fn move_mail_jobs(tx: &Transaction<'_>, collection_id: &str, table: &str) -> Result<usize> {
    let cols = column_names(tx, "main", table)?;
    let has = |name: &str| cols.iter().any(|c| c.eq_ignore_ascii_case(name));
    if !has("id") || !has("worker") || !has("args") {
        return Ok(0);
    }
    // CASE, because json_type fails on text that is not JSON.
    let mail = "worker = ?2 AND CASE WHEN json_valid(args)
                THEN json_type(args, '$.body') IS NOT NULL ELSE 0 END";
    let t = quote(table);
    let moved = tx.execute(
        &format!(
            "INSERT OR REPLACE INTO writer._worker_args (collection, job, args)
             SELECT ?1, id, args FROM main.{t} WHERE {mail}"
        ),
        params![collection_id, GO_MAIL_WORKER],
    )?;
    tx.execute(
        &format!("UPDATE main.{t} SET args = '{{}}' WHERE {mail}"),
        params![collection_id, GO_MAIL_WORKER],
    )?;
    Ok(moved)
}

/// What [`move_credentials`] did.
struct MovedCredentials {
    records: usize,
    indexes_dropped: Vec<String>,
}

/// Copies the credential columns of the Go auth table `table` into
/// `writer._auth` and drops them from the table, with any index that uses
/// them. Reset tokens are stored hashed, as this engine looks them up.
fn move_credentials(
    tx: &Transaction<'_>,
    collection_id: &str,
    table: &str,
) -> Result<MovedCredentials> {
    let mut out = MovedCredentials {
        records: 0,
        indexes_dropped: Vec::new(),
    };
    let cols = column_names(tx, "main", table)?;
    let present: Vec<&str> = GO_SECRET_COLUMNS
        .iter()
        .copied()
        .filter(|s| cols.iter().any(|c| c.eq_ignore_ascii_case(s)))
        .collect();
    if present.is_empty() {
        return Ok(out);
    }
    let into: Vec<String> = present.iter().map(|c| quote(c)).collect();
    let select: Vec<String> = present
        .iter()
        .map(|c| {
            if *c == "otpAttempts" {
                format!("COALESCE({}, 0)", quote(c))
            } else {
                quote(c)
            }
        })
        .collect();
    out.records = tx.execute(
        &format!(
            "INSERT OR REPLACE INTO writer._auth (collection, record, {}) SELECT ?1, id, {} FROM main.{}",
            into.join(", "),
            select.join(", "),
            quote(table)
        ),
        [collection_id],
    )?;
    if present.contains(&"resetToken") {
        hash_reset_tokens(tx, collection_id)?;
    }

    // SQLite refuses to drop a column an index uses.
    for index in indexes_using(tx, table, &present)? {
        tx.execute_batch(&format!("DROP INDEX main.{}", quote(&index)))?;
        out.indexes_dropped.push(index);
    }
    // It also refuses one that a view, a CHECK constraint or a generated
    // column uses. A trigger that uses one would fail each time it fires,
    // so it is looked for first. The Go engine makes none of these, and
    // dropping them would change what the app does, so the conversion
    // stops and says what keeps the column.
    let cannot_move = |col: &str, why: &dyn std::fmt::Display| {
        Error::Conflict(format!(
            "cannot move the credential column {col:?} of {table:?} to the writer-only \
             store: {why}. Remove what uses it with the Go engine or the sqlite3 shell, \
             then open the database again"
        ))
    };
    for col in present {
        let triggers = triggers_using(tx, table, col)?;
        if !triggers.is_empty() {
            let why = format!("trigger {} uses it", triggers.join(", "));
            return Err(cannot_move(col, &why));
        }
        tx.execute_batch(&format!(
            "ALTER TABLE main.{} DROP COLUMN {}",
            quote(table),
            quote(col)
        ))
        .map_err(|e| match e.sqlite_error_code() {
            Some(rusqlite::ErrorCode::Unknown) => cannot_move(col, &e),
            _ => Error::Sqlite(e),
        })?;
    }
    Ok(out)
}

/// The triggers in the shared store that use the column `col` of `table`,
/// by name. SQLite rewrites exactly those when the column is renamed, so a
/// rename that is undone at once finds them. If the rename fails, none
/// are found; dropping the column then fails with SQLite's reason.
fn triggers_using(tx: &Transaction<'_>, table: &str, col: &str) -> Result<Vec<String>> {
    let triggers = || -> rusqlite::Result<Vec<(String, Option<String>)>> {
        tx.prepare("SELECT name, sql FROM main.sqlite_schema WHERE type = 'trigger' ORDER BY name")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    };
    let before = triggers()?;
    if before.is_empty() {
        return Ok(Vec::new());
    }
    tx.execute_batch("SAVEPOINT moul_probe")?;
    let renamed = tx
        .execute_batch(&format!(
            "ALTER TABLE main.{} RENAME COLUMN {} TO __moul_probe",
            quote(table),
            quote(col)
        ))
        .and_then(|()| triggers());
    tx.execute_batch("ROLLBACK TO moul_probe; RELEASE moul_probe")?;
    let Ok(after) = renamed else {
        return Ok(Vec::new());
    };
    Ok(before
        .into_iter()
        .filter(|b| !after.contains(b))
        .map(|(name, _)| name)
        .collect())
}

/// Replaces the plain reset tokens of `collection_id` in `writer._auth`
/// with their SHA-256, in lowercase hex, which is how this engine stores
/// and looks them up. A reset link sent before the conversion keeps
/// working, and the writer-only store holds no usable token.
fn hash_reset_tokens(tx: &Transaction<'_>, collection_id: &str) -> Result<()> {
    let tokens: Vec<(String, String)> = tx
        .prepare(
            "SELECT record, CAST(resetToken AS TEXT) FROM writer._auth
             WHERE collection = ?1 AND resetToken IS NOT NULL AND resetToken <> ''",
        )?
        .query_map([collection_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (record, token) in tokens {
        tx.execute(
            "UPDATE writer._auth SET resetToken = ?3 WHERE collection = ?1 AND record = ?2",
            params![collection_id, record, sha256_hex(token.as_bytes())],
        )?;
    }
    Ok(())
}

/// The lowercase hex SHA-256 of `data`.
fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The indexes on `main.table` that use one of `columns`, by name: as a
/// key column, or in the expression or condition of an expression or
/// partial index. Names are compared whole and without case, so an index
/// on `otpCodeHint` is not one on `otpCode`. Sorted by name.
fn indexes_using(tx: &Transaction<'_>, table: &str, columns: &[&str]) -> Result<Vec<String>> {
    let indexes: Vec<(String, String)> = tx
        .prepare(
            "SELECT name, sql FROM main.sqlite_schema
             WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL ORDER BY name",
        )?
        .query_map([table], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let partial: HashSet<String> = tx
        .prepare(&format!("PRAGMA main.index_list({})", quote(table)))?
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, bool>(4)?)))?
        .filter_map(|r| match r {
            Ok((name, true)) => Some(Ok(name)),
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<rusqlite::Result<_>>()?;
    let wanted = |name: &str| columns.iter().any(|c| c.eq_ignore_ascii_case(name));
    let mut out = Vec::new();
    for (name, sql) in indexes {
        // cid -2 marks an expression; its name is NULL.
        let keys: Vec<(i64, Option<String>)> = tx
            .prepare(&format!("PRAGMA main.index_xinfo({})", quote(&name)))?
            .query_map([], |r| Ok((r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let by_key = keys.iter().any(|(_, k)| k.as_deref().is_some_and(wanted));
        let has_sql = keys.iter().any(|(cid, _)| *cid == -2) || partial.contains(&name);
        if by_key || (has_sql && index_body_names(&sql).iter().any(|n| wanted(n))) {
            out.push(name);
        }
    }
    Ok(out)
}

/// The names in the column list and condition of a `CREATE INDEX`
/// statement: every bare or quoted name after the first `(`, so not the
/// index's or the table's. Quotes are taken off; string literals and
/// comments are skipped.
fn index_body_names(sql: &str) -> Vec<String> {
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii();
    let mut out = Vec::new();
    let mut body = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        let name = match c {
            '(' => {
                body = true;
                continue;
            }
            '\'' | '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let mut text = String::new();
                while let Some(n) = chars.next() {
                    if n != close {
                        text.push(n);
                    } else if close != ']' && chars.peek() == Some(&close) {
                        // A doubled quote stands for itself.
                        text.push(chars.next().unwrap_or(close));
                    } else {
                        break;
                    }
                }
                if c == '\'' {
                    continue;
                }
                text
            }
            '-' if chars.peek() == Some(&'-') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                continue;
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut star = false;
                for n in chars.by_ref() {
                    if star && n == '/' {
                        break;
                    }
                    star = n == '*';
                }
                continue;
            }
            c if is_name(c) => {
                let mut text = String::from(c);
                while let Some(&n) = chars.peek() {
                    if !is_name(n) {
                        break;
                    }
                    text.push(n);
                    chars.next();
                }
                text
            }
            _ => continue,
        };
        if body {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_database_gets_all_system_tables() {
        let mut s = Store::open_in_memory().unwrap();
        let m = s.migrate(|| unreachable!()).unwrap();
        assert!(m.created);
        assert_eq!(s.format().unwrap(), FORMAT);
        for (schema, table) in [
            ("main", "_moul"),
            ("main", "_changes"),
            ("main", "_files"),
            ("writer", "_auth"),
            ("writer", "_inbox"),
            ("device", "_device"),
        ] {
            assert!(
                has_table(s.conn(), schema, table).unwrap(),
                "{schema}.{table}"
            );
        }
        // Migrating again does nothing.
        assert_eq!(s.migrate(|| unreachable!()).unwrap(), Migrated::default());
        // A store made before the change log carried effects gets the column.
        s.conn()
            .execute_batch("ALTER TABLE main._changes DROP COLUMN effects")
            .unwrap();
        s.migrate(|| unreachable!()).unwrap();
        assert!(
            column_names(s.conn(), "main", "_changes")
                .unwrap()
                .contains(&"effects".to_owned())
        );
    }

    #[test]
    fn index_body_names_are_whole_names_after_the_table() {
        assert_eq!(
            index_body_names(
                "CREATE INDEX \"idx (otpCode\" ON otpCode (lower(\"pass\"\"Keys\"), [reset Token], `x`) \
                 WHERE a <> 'otpCode' -- otpCode\n AND /* otpCode */ otpCodeHint IS NOT NULL"
            ),
            [
                "lower",
                "pass\"Keys",
                "reset Token",
                "x",
                "WHERE",
                "a",
                "AND",
                "otpCodeHint",
                "IS",
                "NOT",
                "NULL"
            ]
        );
    }

    #[test]
    fn refuses_a_newer_format() {
        let mut s = Store::open_in_memory().unwrap();
        s.conn()
            .execute_batch("PRAGMA main.user_version = 99")
            .unwrap();
        assert!(matches!(
            s.migrate(|| unreachable!()),
            Err(Error::TooNew(_))
        ));
        assert!(matches!(s.migrate_local(), Err(Error::TooNew(_))));
    }
}
