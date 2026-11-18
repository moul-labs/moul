//! A fence around SQL that comes from rules.
//!
//! Rules and filters compile to SQL, and a rule's `exists(` condition is SQL
//! its author wrote. That SQL runs on the engine's connection, where the
//! writer-only store (credentials, keys) and the device store (sync keys)
//! are attached. While the guard is up, SQLite's authorizer lets a statement
//! being prepared only read the shared store: no other schema, no writes,
//! no pragmas, no attaching.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

/// The switch, and the tables of the shared store when it went up. Clones
/// share both.
#[derive(Clone, Debug, Default)]
pub(crate) struct SqlGuard {
    on: Arc<AtomicBool>,
    shared_tables: Arc<Mutex<HashSet<String>>>,
}

impl SqlGuard {
    /// Installs the authorizer on `conn`, driven by this switch. The store
    /// calls it on every new connection, so the fence survives a reopen.
    pub(crate) fn install_on(&self, conn: &mut Connection) -> rusqlite::Result<()> {
        let on = self.on.clone();
        let tables = self.shared_tables.clone();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if !on.load(Ordering::Relaxed) {
                return Authorization::Allow;
            }
            let tables = tables.lock().expect("the guard's lock is never poisoned");
            decide(&ctx, &tables)
        }))
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
        let was = self.on.swap(true, Ordering::Relaxed);
        let out = f();
        self.on.store(was, Ordering::Relaxed);
        Ok(out)
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
}
