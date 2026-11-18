//! A database the Go engine made converts into three stores on first open.

#![cfg(not(target_arch = "wasm32"))]

use moul_fixtures::Fixtures;
use moul_store::schema::CollectionKind;
use moul_store::{Store, catalog};
use moul_vfs::{CrashMode, MemoryStorage, Volume};

/// The seeded Go database: an auth collection with password hashes, base,
/// worker and analytic collections, a root user, settings and logs.
const SEEDED: &str = "cbc2a77b5c6e0079";

fn go_store(state: &str) -> Store {
    let store = Store::open_in_memory().unwrap();
    Fixtures::default()
        .db(state)
        .unwrap()
        .load_into(store.conn())
        .unwrap();
    store
}

fn count(store: &Store, sql: &str) -> i64 {
    store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn has_column(store: &Store, schema: &str, table: &str, col: &str) -> bool {
    count(
        store,
        &format!(
            "SELECT count(*) FROM pragma_table_info('{table}', '{schema}') WHERE name = '{col}'"
        ),
    ) > 0
}

/// A rendered one-time code, as the Go engine queued it for sending.
const OTP_MAIL: &str = "Your one-time password (OTP) code is: 482913";
/// A rendered reset link, as the Go engine queued it for sending.
const RESET_MAIL: &str = "reset-password?token=plaintextResetToken42";

/// Queues two mails the way the Go engine did: one in `_workers`, one in
/// the user worker collection `tasks_queue`.
fn queue_mail(store: &Store) {
    for (table, body) in [("_workers", OTP_MAIL), ("tasks_queue", RESET_MAIL)] {
        store
            .conn()
            .execute(
                &format!(
                    "INSERT INTO main.{table} (id, createdAt, updatedAt, worker, args, inserted_at, scheduled_at)
                     VALUES (?1, 'x', 'x', 'SendEmail', ?2, 'x', 'x')"
                ),
                [
                    format!("job_{table}"),
                    serde_json::json!({"to": "a@example.com", "subject": "Mail", "body": body})
                        .to_string(),
                ],
            )
            .unwrap();
    }
}

fn contains(file: &[u8], needle: &str) -> bool {
    file.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// The id and arguments of every job in `schema.table`, by id.
fn job_args(store: &Store, schema: &str, table: &str) -> Vec<(String, String)> {
    let id = if schema == "writer" { "job" } else { "id" };
    store
        .conn()
        .prepare(&format!(
            "SELECT {id}, args FROM {schema}.{table} ORDER BY {id}"
        ))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn seeded_go_database_converts() {
    let mut store = go_store(SEEDED);
    queue_mail(&store);
    // A mail job a user queued, with arguments that are not JSON.
    store
        .conn()
        .execute(
            "INSERT INTO main.tasks_queue (id, createdAt, updatedAt, worker, args, inserted_at, scheduled_at)
             VALUES ('job_user_plain', 'x', 'x', 'SendEmail', 'not json', 'x', 'x')",
            [],
        )
        .unwrap();
    let jobs_before = job_args(&store, "main", "tasks_queue");
    let users_before = count(&store, "SELECT count(*) FROM main.users");
    let hash_before: String = store
        .conn()
        .query_row(
            "SELECT passwordHash FROM main.users WHERE id = 'usr_admin_001'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let mut n = 0;
    let migrated = store
        .migrate(|| {
            n += 1;
            format!("fld_{n:08}")
        })
        .unwrap();
    let conv = migrated.converted_from_go.expect("converted");
    assert_eq!(conv.credentials_moved as i64, users_before);
    assert!(conv.tables_moved.contains(&"_settings".to_owned()));
    assert!(conv.tables_moved.contains(&"_rootUsers".to_owned()));
    assert_eq!(conv.tables_dropped, ["_realtime_tickets"]);
    assert!(conv.tables_moved.contains(&"_workers".to_owned()));
    // Only the reset mail the engine rendered; the fixture's welcome job
    // has no body.
    assert_eq!(conv.mail_jobs_moved, 1);

    // Credentials left the shared store and kept their values.
    assert!(!has_column(&store, "main", "users", "passwordHash"));
    assert!(!has_column(&store, "main", "users", "resetToken"));
    let hash_after: String = store
        .conn()
        .query_row(
            "SELECT a.passwordHash FROM writer._auth a JOIN main._moul m ON m.id = a.collection
             WHERE m.name = 'users' AND a.record = 'usr_admin_001'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hash_after, hash_before);
    assert_eq!(
        count(&store, "SELECT count(*) FROM main.users"),
        users_before
    );
    assert_eq!(count(&store, "SELECT count(*) FROM writer._rootUsers"), 1);
    assert!(count(&store, "SELECT count(*) FROM writer._settings") > 0);
    for gone in [
        "_settings",
        "_rootUsers",
        "_requests",
        "_visits",
        "_realtime_tickets",
        "_certmagic",
        "_workers",
    ] {
        assert_eq!(
            count(
                &store,
                &format!("SELECT count(*) FROM main.sqlite_schema WHERE name = '{gone}'")
            ),
            0,
            "{gone} should have left the shared store"
        );
    }

    // Every field has an id now, and every collection still reads.
    let all = catalog::load_all(store.conn()).unwrap();
    assert_eq!(all.len(), 5);
    assert!(
        all.iter()
            .flat_map(|c| &c.fields)
            .all(|f| f.id.starts_with("fld_"))
    );
    assert!(all.iter().any(|c| c.kind == CollectionKind::Worker));
    assert_eq!(store.format().unwrap(), moul_store::FORMAT);

    // No password hash survives anywhere in the shared file, not even in
    // free pages.
    let file = store.export().unwrap();
    assert!(
        !file
            .windows(hash_before.len())
            .any(|w| w == hash_before.as_bytes()),
        "the shared store still holds a password hash"
    );

    // Rendered mail left the shared store: the Go jobs table moved, and
    // the engine's mail job in a worker collection moved its arguments to
    // the writer-only store. Every other job kept its arguments.
    assert_eq!(count(&store, "SELECT count(*) FROM writer._workers"), 1);
    let rendered = jobs_before
        .iter()
        .find(|(id, _)| id == "job_tasks_queue")
        .unwrap()
        .clone();
    let expected: Vec<(String, String)> = jobs_before
        .iter()
        .map(|(id, args)| {
            let args = if *id == rendered.0 { "{}" } else { args };
            (id.clone(), args.to_owned())
        })
        .collect();
    assert_eq!(job_args(&store, "main", "tasks_queue"), expected);
    assert!(expected.contains(&(
        "job_email_001".into(),
        r#"{"to": "jane.doe@example.com", "subject": "Welcome to Moul!"}"#.into()
    )));
    assert_eq!(job_args(&store, "writer", "_worker_args"), [rendered]);
    for mail in [OTP_MAIL, RESET_MAIL] {
        assert!(
            !contains(&file, mail),
            "the shared store still holds {mail:?}"
        );
    }

    // Opening again changes nothing.
    assert_eq!(
        store.migrate(|| unreachable!()).unwrap(),
        Default::default()
    );
}

#[test]
fn every_recorded_go_database_converts() {
    let fx = Fixtures::default();
    let mut ids: Vec<String> = std::fs::read_dir(fx.dir().join("db"))
        .unwrap()
        .map(|e| {
            e.unwrap()
                .path()
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    ids.sort();
    for id in ids {
        let mut store = go_store(&id);
        let mut n = 0;
        store
            .migrate(|| {
                n += 1;
                format!("fld_{n}")
            })
            .unwrap_or_else(|e| panic!("{id}: {e}"));
        let integrity: String = store
            .conn()
            .query_row("PRAGMA main.integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok", "{id}");
        catalog::load_all(store.conn()).unwrap_or_else(|e| panic!("{id}: {e}"));
    }
}

/// A Go database with a pending password reset: alice's token is
/// `reset-alice`.
const RESET_PENDING: &str = "192c81da234ca01e";

#[test]
fn reset_tokens_move_hashed() {
    let mut store = go_store(RESET_PENDING);
    let mut n = 0;
    store
        .migrate(|| {
            n += 1;
            format!("fld_{n}")
        })
        .unwrap();
    let token: String = store
        .conn()
        .query_row(
            "SELECT resetToken FROM writer._auth WHERE record = 'alice'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // Lowercase hex SHA-256 of "reset-alice", as the engine looks it up.
    assert_eq!(
        token,
        "17af8cff71098ff048b42d40d8204e4422cf3d8247570e380fa4b0ef26b0a44c"
    );
    assert!(!contains(&store.export().unwrap(), "reset-alice"));
}

#[test]
fn indexes_on_credentials_do_not_block_the_conversion() {
    let mut store = go_store(RESET_PENDING);
    store
        .conn()
        .execute_batch(
            "ALTER TABLE users ADD COLUMN otpCodeHint TEXT;
             CREATE INDEX idx_users_reset ON users (resetToken);
             CREATE INDEX idx_users_otp ON users (displayName) WHERE otpCode IS NOT NULL;
             CREATE INDEX idx_users_keys ON users (lower(\"PassKeys\"));
             CREATE INDEX idx_users_name ON users (displayName);
             CREATE UNIQUE INDEX idx_users_otpCodeHint_unique ON users (otpCodeHint);
             CREATE INDEX idx_users_hint ON users (lower(otpCodeHint)) WHERE otpCodeHint <> 'otpCode';",
        )
        .unwrap();
    let mut n = 0;
    let conv = store
        .migrate(|| {
            n += 1;
            format!("fld_{n}")
        })
        .unwrap()
        .converted_from_go
        .unwrap();
    assert_eq!(
        conv.indexes_dropped,
        ["idx_users_keys", "idx_users_otp", "idx_users_reset"]
    );
    assert!(!has_column(&store, "main", "users", "resetToken"));
    // Names that only contain a credential column's name are other columns.
    for kept in [
        "idx_users_name",
        "idx_users_otpCodeHint_unique",
        "idx_users_hint",
    ] {
        assert_eq!(
            count(
                &store,
                &format!("SELECT count(*) FROM main.sqlite_schema WHERE name = '{kept}'")
            ),
            1,
            "{kept}"
        );
    }
    // The unique index still holds.
    let err = store
        .conn()
        .execute_batch(
            "UPDATE main.users SET otpCodeHint = 'same';
             INSERT INTO main.users (id, createdAt, updatedAt, username, email, otpCodeHint)
             VALUES ('bob', 'x', 'x', 'bob', 'bob@example.com', 'same');",
        )
        .unwrap_err();
    assert!(err.to_string().contains("UNIQUE"), "{err}");
}

/// Converts the Go database `state` after running `sql` on it.
fn convert_after(state: &str, sql: &str) -> (Store, moul_store::Result<()>) {
    let mut store = go_store(state);
    store.conn().execute_batch(sql).unwrap();
    let mut n = 0;
    let converted = store
        .migrate(|| {
            n += 1;
            format!("fld_{n}")
        })
        .map(drop);
    (store, converted)
}

#[test]
fn what_else_uses_a_credential_column_stops_the_conversion_with_a_clear_error() {
    for (sql, names) in [
        (
            "CREATE VIEW codes AS SELECT id, otpCode FROM users;",
            "view codes",
        ),
        (
            "CREATE TRIGGER clear_codes AFTER UPDATE OF email ON users
             BEGIN UPDATE users SET otpCode = NULL WHERE id = new.id; END;",
            "trigger clear_codes",
        ),
        (
            "CREATE TABLE audit (id TEXT, otpCode TEXT);
             CREATE TRIGGER audit_codes AFTER INSERT ON audit
             BEGIN UPDATE users SET otpCode = new.otpCode WHERE id = new.id; END;",
            "trigger audit_codes",
        ),
        (
            "ALTER TABLE users ADD COLUMN c TEXT CHECK (otpCode IS NULL OR c IS NULL);",
            "table users",
        ),
        (
            "ALTER TABLE users ADD COLUMN g TEXT AS (otpCode);",
            "table users",
        ),
    ] {
        let (store, converted) = convert_after(RESET_PENDING, sql);
        let err = converted.unwrap_err();
        assert!(
            matches!(err, moul_store::Error::Conflict(_)),
            "{sql}: {err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains("\"otpCode\"") && text.contains(names),
            "{sql}: {text}"
        );
        // Nothing changed: the next open tries again.
        assert_eq!(store.format().unwrap(), 0, "{sql}");
        assert!(has_column(&store, "main", "users", "otpCode"), "{sql}");
    }

    // A trigger on another table's own `otpCode` column is no obstacle.
    let (store, converted) = convert_after(
        RESET_PENDING,
        "CREATE TABLE audit (id TEXT, otpCode TEXT);
         CREATE TRIGGER audit_own AFTER INSERT ON audit
         BEGIN UPDATE audit SET otpCode = NULL WHERE id = new.id; END;",
    );
    converted.unwrap();
    assert!(!has_column(&store, "main", "users", "otpCode"));
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM main.sqlite_schema WHERE name = 'audit_own'"
        ),
        1
    );
}

/// Opens the Go database `state` on `storage`, then closes it.
fn go_storage(state: &str) -> MemoryStorage {
    let storage = MemoryStorage::new();
    let store = Store::open(Volume::open(storage.clone()).unwrap()).unwrap();
    Fixtures::default()
        .db(state)
        .unwrap()
        .load_into(store.conn())
        .unwrap();
    store.close().unwrap();
    storage
}

#[test]
fn an_interrupted_scrub_finishes_on_the_next_open() {
    let base = go_storage(SEEDED);
    let hash: String = {
        let store = Store::open(Volume::open(base.fork()).unwrap()).unwrap();
        store
            .conn()
            .query_row(
                "SELECT passwordHash FROM main.users WHERE id = 'usr_admin_001'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let ids = || {
        let mut n = 0;
        move || {
            n += 1;
            format!("fld_{n}")
        }
    };

    // How many steps a whole migration takes.
    let total = {
        let storage = base.fork();
        let mut store = Store::open(Volume::open(storage.clone()).unwrap()).unwrap();
        let start = storage.steps();
        store.migrate(ids()).unwrap();
        storage.steps() - start
    };

    // Fail each step in turn. Whatever was cut short, the next open
    // finishes the conversion and the scrub.
    let mut scrub_interrupted = 0;
    for step in 0..total {
        let storage = base.fork();
        let mut store = Store::open(Volume::open(storage.clone()).unwrap()).unwrap();
        storage.fail_at(Some(storage.steps() + step));
        if store.migrate(ids()).is_ok() {
            continue;
        }
        drop(store);
        storage.crash(CrashMode::DropUnflushed);
        let mut store = Store::open(Volume::open(storage.clone()).unwrap()).unwrap();
        if store.format().unwrap() == moul_store::FORMAT {
            scrub_interrupted += 1;
        }
        store
            .migrate(ids())
            .unwrap_or_else(|e| panic!("step {step}: {e}"));
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM main._meta WHERE key = 'scrub'"
            ),
            0,
            "step {step}"
        );
        // VACUUM ran: no free page is left to hold old bytes.
        assert_eq!(
            count(&store, "PRAGMA main.freelist_count"),
            0,
            "step {step}"
        );
        assert!(
            !contains(&store.export().unwrap(), &hash),
            "step {step}: the shared store still holds a password hash"
        );
    }
    assert!(
        scrub_interrupted > 0,
        "no step fell between the conversion and its scrub"
    );
}
