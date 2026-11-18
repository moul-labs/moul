//! A fence around SQL that comes from rules, and around the owner's queries.
//!
//! Rules and filters compile to SQL, and a rule's `exists(` condition is SQL
//! its author wrote. That SQL runs on the engine's connection, where the
//! writer-only store (credentials, keys) and the device store (sync keys)
//! are attached. While the guard is up, SQLite's authorizer lets a statement
//! being prepared only read the shared store: no other schema, no writes,
//! no pragmas, no attaching.
//!
//! The same authorizer has a second, looser mode for SQL the owner runs to
//! read ([`Engine::read_only`](crate::Engine::read_only)): any store may be
//! read, and pragmas may be read, but nothing may be written or set, no
//! database attached or detached, and no transaction begun or ended.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

/// The authorizer lets everything through.
const OFF: u8 = 0;
/// Rule SQL: only reads of the shared store.
const RULES: u8 = 1;
/// The owner's queries: only reads, of any store.
const READ: u8 = 2;

/// The switch, and the tables of the shared store when it went up. Clones
/// share both.
#[derive(Clone, Debug, Default)]
pub(crate) struct SqlGuard {
    mode: Arc<AtomicU8>,
    shared_tables: Arc<Mutex<HashSet<String>>>,
}

/// Puts the mode back as it was when dropped, even if what ran panicked.
struct Restore<'a> {
    mode: &'a AtomicU8,
    was: u8,
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        self.mode.store(self.was, Ordering::Relaxed);
    }
}

impl SqlGuard {
    /// Installs the authorizer on `conn`, driven by this switch. The store
    /// calls it on every new connection, so the fence survives a reopen.
    pub(crate) fn install_on(&self, conn: &mut Connection) -> rusqlite::Result<()> {
        let mode = self.mode.clone();
        let tables = self.shared_tables.clone();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            match mode.load(Ordering::Relaxed) {
                OFF => Authorization::Allow,
                RULES => {
                    let tables = tables.lock().expect("the guard's lock is never poisoned");
                    decide(&ctx, &tables)
                }
                _ => decide_read(&ctx),
            }
        }))
    }

    /// Switches to `mode` until the returned value is dropped.
    fn enter(&self, mode: u8) -> Restore<'_> {
        Restore {
            mode: &self.mode,
            was: self.mode.swap(mode, Ordering::Relaxed),
        }
    }

    /// Runs `f` with the guard up. `conn` lists the shared store's tables
    /// first: SQLite names no schema for a table read without columns, so
    /// such a read is allowed only for a name the shared store has.
    pub(crate) fn run<T>(&self, conn: &Connection, f: impl FnOnce() -> T) -> rusqlite::Result<T> {
        let names = conn
            .prepare(
                "SELECT name FROM main.sqlite_schema WHERE type IN ('table', 'view')
                 AND name NOT LIKE 'sqlite_%'",
            )?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<String>>>()?;
        *self.shared_tables.lock().expect("never poisoned") = names;
        let _restore = self.enter(RULES);
        Ok(f())
    }

    /// Runs `f` with the guard in its reading mode: statements prepared or
    /// run meanwhile may read any store and read pragmas, and nothing else.
    pub(crate) fn read_only<T>(&self, f: impl FnOnce() -> T) -> T {
        let _restore = self.enter(READ);
        f()
    }
}

/// Pragmas that take an argument and still only read.
const PRAGMAS_READ_WITH_ARGUMENT: &[&str] = &[
    "foreign_key_check",
    "foreign_key_list",
    "index_info",
    "index_list",
    "index_xinfo",
    "integrity_check",
    "quick_check",
    "table_info",
    "table_list",
    "table_xinfo",
];

/// Pragmas that act even without a value.
const PRAGMAS_THAT_ACT: &[&str] = &["incremental_vacuum", "optimize", "wal_checkpoint"];

fn decide_read(ctx: &AuthContext<'_>) -> Authorization {
    let allowed = match ctx.action {
        AuthAction::Select | AuthAction::Recursive | AuthAction::Read { .. } => true,
        AuthAction::Function { function_name } => {
            !function_name.eq_ignore_ascii_case("load_extension")
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => {
            let is = |list: &[&str]| list.iter().any(|p| p.eq_ignore_ascii_case(pragma_name));
            match pragma_value {
                None => !is(PRAGMAS_THAT_ACT),
                Some(_) => is(PRAGMAS_READ_WITH_ARGUMENT),
            }
        }
        _ => false,
    };
    if allowed {
        Authorization::Allow
    } else {
        Authorization::Deny
    }
}

fn decide(ctx: &AuthContext<'_>, shared_tables: &HashSet<String>) -> Authorization {
    match ctx.action {
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Read { table_name, .. } => {
            // Judge by name, not by the schema SQLite reports: eponymous
            // virtual tables such as sqlite_dbpage say "main" whatever they
            // read. Only the shared store's own tables, and the JSON table
            // functions the rule compiler uses, may be read.
            let named = matches!(ctx.database_name, None | Some("main"));
            let allowed = (named && shared_tables.contains(table_name))
                || table_name.eq_ignore_ascii_case("json_each")
                || table_name.eq_ignore_ascii_case("json_tree");
            if allowed {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } => {
            if function_name.eq_ignore_ascii_case("load_extension") {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }
        _ => Authorization::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moul_store::Store;

    #[test]
    fn rule_sql_cannot_leave_the_shared_store() {
        let mut store = Store::open_in_memory().unwrap();
        store.migrate(|| unreachable!()).unwrap();
        store
            .conn()
            .execute_batch(
                "INSERT INTO writer._keys (name, value) VALUES ('token', x'00');
                 CREATE TABLE device.moul_sync (key TEXT, value TEXT);",
            )
            .unwrap();
        let guard = SqlGuard::default();
        guard.install_on(store.conn_mut()).unwrap();
        let conn = store.conn();
        let try_sql = |sql: &str| guard.run(conn, || conn.prepare(sql).map(|_| ())).unwrap();
        assert!(try_sql("SELECT count(*) FROM _moul WHERE id = ?1").is_ok());
        assert!(try_sql("SELECT 1 FROM _moul").is_ok());
        assert!(try_sql("WITH t(x) AS (SELECT 1) SELECT x FROM t").is_ok());
        assert!(try_sql("SELECT 1 FROM _moul WHERE EXISTS (SELECT 1 FROM writer._keys)").is_err());
        // Unqualified names that only exist in another store are refused too.
        assert!(try_sql("SELECT 1 FROM moul_sync").is_err());
        assert!(try_sql("DELETE FROM _moul").is_err());
        assert!(try_sql("PRAGMA writer.user_version").is_err());
        assert!(try_sql("ATTACH ':memory:' AS x").is_err());
        // Virtual tables that read raw pages or internals are refused even
        // though SQLite reports them as "main".
        assert!(try_sql("SELECT 1 FROM pragma_table_info('_auth', 'writer')").is_err());
        assert!(try_sql("SELECT name FROM sqlite_schema").is_err());
        assert!(try_sql("SELECT value FROM json_each('[1]')").is_ok());
        // With the guard down, the engine reads its own stores.
        assert!(conn.prepare("SELECT value FROM writer._keys").is_ok());
    }

    #[test]
    fn owner_queries_only_read() {
        let mut store = Store::open_in_memory().unwrap();
        store.migrate(|| unreachable!()).unwrap();
        let guard = SqlGuard::default();
        guard.install_on(store.conn_mut()).unwrap();
        let conn = store.conn();
        conn.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');")
            .unwrap();
        let try_sql = |sql: &str| {
            guard.read_only(|| {
                let mut stmt = conn.prepare(sql)?;
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok::<_, rusqlite::Error>(())
            })
        };
        for sql in [
            "PRAGMA query_only = 1",
            "PRAGMA foreign_keys = OFF",
            "PRAGMA writable_schema = 1",
            "PRAGMA journal_mode = DELETE",
            "PRAGMA locking_mode = EXCLUSIVE",
            "PRAGMA writer.user_version = 7",
            "PRAGMA optimize",
            "ATTACH ':memory:' AS x",
            "DETACH device",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "SAVEPOINT a",
            "RELEASE a",
            "DELETE FROM t",
            "CREATE TEMP TABLE u (a)",
            "SELECT load_extension('x')",
        ] {
            // SQLite says "not authorized", with SQLITE_AUTH except for a
            // function it refuses.
            let e = try_sql(sql).expect_err(sql);
            assert!(e.to_string().contains("not authorized"), "{sql}: {e}");
        }
        for sql in [
            "PRAGMA page_size",
            "PRAGMA query_only",
            "PRAGMA writer.user_version",
            "PRAGMA table_info(t)",
            "SELECT v FROM t",
            "SELECT value FROM writer._keys",
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 3) SELECT x FROM n",
            "SELECT * FROM pragma_table_info('t')",
        ] {
            assert!(try_sql(sql).is_ok(), "{sql}: {:?}", try_sql(sql));
        }
        // Nothing was set, attached or left open, and the guard is down again.
        assert!(conn.is_autocommit());
        let query_only: i64 = conn
            .query_row("PRAGMA query_only", [], |r| r.get(0))
            .unwrap();
        assert_eq!(query_only, 0);
        conn.execute("INSERT INTO t VALUES ('two')", []).unwrap();
        // The rule fence still works after the reading mode.
        let rule = guard
            .run(conn, || {
                conn.prepare("SELECT value FROM writer._keys").map(|_| ())
            })
            .unwrap();
        assert!(rule.is_err());
    }
}
