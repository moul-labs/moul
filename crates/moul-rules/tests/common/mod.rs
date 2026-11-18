//! Test support shared by the parity suite and the ported Go tests: a
//! [`Lookup`] over an in-memory SQLite database laid out as the Go engine
//! keeps it, with collections in `_moul`.

#![allow(dead_code)]

use moul_rules::{Lookup, Map};
use moul_store::schema::Collection;
use rusqlite::Connection;
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde_json::{Value, json};

/// 2026-11-18 12:34:56.789 UTC, around when the fixtures were recorded.
pub const NOW_MS: i64 = 1_795_005_296_789;

/// A Go engine database: tables named after collections, definitions in
/// `_moul`.
pub struct GoDb {
    pub conn: Connection,
}

impl GoDb {
    pub fn new() -> GoDb {
        GoDb {
            conn: Connection::open_in_memory().expect("open in-memory database"),
        }
    }

    /// Stores `c` in `_moul`, creating the table if needed.
    pub fn register(&self, c: &Collection) {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS _moul (id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, type TEXT NOT NULL,
                 fields TEXT NOT NULL, rules TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL)",
            )
            .unwrap();
        self.conn
            .execute(
                "INSERT INTO _moul VALUES (?1, ?2, ?3, ?4, ?5, '', '')",
                (
                    &c.id,
                    &c.name,
                    c.kind.as_str(),
                    serde_json::to_string(&c.fields).unwrap(),
                    serde_json::to_string(&c.rules).unwrap(),
                ),
            )
            .unwrap();
    }
}

impl Lookup for GoDb {
    fn collection(&self, name: &str) -> Option<Collection> {
        let (id, kind, fields, rules): (String, String, String, String) = self
            .conn
            .query_row(
                "SELECT id, type, fields, rules FROM _moul WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()?;
        let fields: Value = serde_json::from_str(&fields).ok()?;
        let rules: Value = serde_json::from_str(&rules).ok()?;
        serde_json::from_value(
            json!({"id": id, "name": name, "type": kind, "fields": fields, "rules": rules}),
        )
        .ok()
    }

    fn first_row(&self, sql: &str, params: &[Value]) -> Result<Option<Map>, String> {
        let mut stmt = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
        let values: Vec<SqlValue> = params.iter().map(to_sql).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(values))
            .map_err(|e| e.to_string())?;
        let Some(row) = rows.next().map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let mut out = Map::new();
        for (i, name) in names.iter().enumerate() {
            let v = match row.get_ref(i).map_err(|e| e.to_string())? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(f) => json!(f),
                ValueRef::Text(t) | ValueRef::Blob(t) => {
                    Value::String(String::from_utf8_lossy(t).into_owned())
                }
            };
            out.insert(name.clone(), v);
        }
        Ok(Some(out))
    }
}

/// A JSON value bound as SQLite binds Go values.
pub fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// Binds a compiled filter's named parameters.
pub fn named_params(params: &[(String, Value)]) -> Vec<(String, SqlValue)> {
    params
        .iter()
        .map(|(k, v)| (format!(":{k}"), to_sql(v)))
        .collect()
}

/// `SELECT id FROM table WHERE <where> ORDER BY id`, as the Go tests' `listIDs`.
pub fn list_ids(
    conn: &Connection,
    table: &str,
    filter: Option<&moul_rules::SqlFilter>,
) -> Vec<String> {
    let (sql, params) = match filter {
        Some(f) => (
            format!("SELECT id FROM `{table}` WHERE {} ORDER BY id", f.sql),
            named_params(&f.params),
        ),
        None => (format!("SELECT id FROM `{table}` ORDER BY id"), Vec::new()),
    };
    let mut stmt = conn
        .prepare(&sql)
        .unwrap_or_else(|e| panic!("prepare {sql}: {e}"));
    let refs: Vec<(&str, &dyn rusqlite::ToSql)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v as &dyn rusqlite::ToSql))
        .collect();
    stmt.query_map(refs.as_slice(), |r| r.get::<_, String>(0))
        .unwrap_or_else(|e| panic!("query {sql}: {e}"))
        .map(Result::unwrap)
        .collect()
}
