//! The engine's side of the rule language: every call into `moul-rules`
//! goes through here, with the database lookups rules need.

use moul_rules::{EvalContext, FilterOptions, Lookup, SortOptions};
use moul_store::schema::Collection;
use rusqlite::Connection;
use rusqlite::types::{Value as Sql, ValueRef};
use serde_json::{Map, Value, json};

use crate::api::{ApiResult, Response};
use crate::engine::{Catalog, Cx};
use crate::guard::SqlGuard;

type Auth<'a> = Option<&'a Map<String, Value>>;

/// A `WHERE` text and its named params.
pub(crate) type SqlWhere = (String, Vec<(String, Value)>);

/// The database as rules see it: collection definitions and one-row reads.
struct Db<'a> {
    conn: &'a Connection,
    catalog: &'a Catalog,
    guard: &'a SqlGuard,
}

impl Lookup for Db<'_> {
    fn collection(&self, name: &str) -> Option<Collection> {
        self.catalog.get(name).cloned()
    }

    fn first_row(&self, sql: &str, params: &[Value]) -> Result<Option<Map<String, Value>>, String> {
        // The SQL may be the rule author's own (`exists(`): fence it in.
        self.guard
            .run(self.conn, || self.read_first_row(sql, params))
            .map_err(|e| e.to_string())?
    }
}

impl Db<'_> {
    fn read_first_row(
        &self,
        sql: &str,
        params: &[Value],
    ) -> Result<Option<Map<String, Value>>, String> {
        let mut stmt = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        for (i, p) in params.iter().enumerate() {
            let v = match p {
                Value::Null => Sql::Null,
                Value::Bool(b) => Sql::Integer(*b as i64),
                Value::Number(n) => match n.as_i64() {
                    Some(i) => Sql::Integer(i),
                    None => Sql::Real(n.as_f64().unwrap_or(0.0)),
                },
                Value::String(s) => Sql::Text(s.clone()),
                other => Sql::Text(other.to_string()),
            };
            stmt.raw_bind_parameter(i + 1, v)
                .map_err(|e| e.to_string())?;
        }
        let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
        let mut rows = stmt.raw_query();
        let Some(row) = rows.next().map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let mut out = Map::new();
        for (i, name) in names.iter().enumerate() {
            let v = match row.get_ref(i).map_err(|e| e.to_string())? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(f) => crate::values::number(f),
                ValueRef::Text(t) | ValueRef::Blob(t) => {
                    Value::String(String::from_utf8_lossy(t).into_owned())
                }
            };
            out.insert(name.clone(), v);
        }
        Ok(Some(out))
    }
}

/// Whether `rule` lets `auth` through for `record`. An empty rule lets
/// everyone through.
pub(crate) fn allows(
    cx: &Cx<'_>,
    rule: &str,
    auth: Auth<'_>,
    record: &Map<String, Value>,
    request: Option<&Map<String, Value>>,
) -> Result<bool, String> {
    allows_db(
        cx.conn, cx.catalog, cx.guard, rule, auth, record, request, cx.now_ms,
    )
}

/// [`allows`] outside a request.
#[allow(clippy::too_many_arguments)]
pub(crate) fn allows_db(
    conn: &Connection,
    catalog: &Catalog,
    guard: &SqlGuard,
    rule: &str,
    auth: Auth<'_>,
    record: &Map<String, Value>,
    request: Option<&Map<String, Value>>,
    now_ms: i64,
) -> Result<bool, String> {
    if rule.is_empty() {
        return Ok(true);
    }
    let db = Db {
        conn,
        catalog,
        guard,
    };
    let ctx = EvalContext {
        auth,
        record,
        request,
        db: Some(&db),
        now_ms,
    };
    moul_rules::evaluate(rule, &ctx).map_err(|e| e.message().to_owned())
}

/// A filter (or list rule) as SQL: the `WHERE` text and its named params.
pub(crate) fn filter(
    cx: &Cx<'_>,
    c: &Collection,
    filter: &str,
    auth: Auth<'_>,
    request: Option<&Map<String, Value>>,
    prefix: &str,
    untrusted: bool,
) -> Result<Option<SqlWhere>, String> {
    let load = |name: &str| cx.catalog.get(name).cloned();
    let opts = FilterOptions {
        param_prefix: prefix,
        untrusted,
        load: Some(&load),
        now_ms: cx.now_ms,
        cloak_hash: None,
    };
    moul_rules::filter_sql(filter, c, auth, request, &opts)
        .map(|f| f.map(|f| (f.sql, f.params)))
        .map_err(|e| e.message().to_owned())
}

/// A sort string as `ORDER BY` terms.
pub(crate) fn sort(
    cx: &Cx<'_>,
    c: &Collection,
    sort: &str,
    untrusted: bool,
) -> Result<Vec<String>, String> {
    let load = |name: &str| cx.catalog.get(name).cloned();
    let opts = SortOptions {
        untrusted,
        load: Some(&load),
    };
    moul_rules::sort_sql(sort, c, &opts).map_err(|e| e.message().to_owned())
}

/// Columns an update rule pins to the caller (`owner = @request.auth.id`).
pub(crate) fn owner_fields(rule: &str) -> Vec<String> {
    moul_rules::owner_fields(rule)
}

/// Whether `name` is a credential column of `c`.
pub(crate) fn is_secret_field(name: &str, c: &Collection) -> bool {
    moul_rules::is_secret_field(name, c)
}

/// Whether `name` is a column of `c`.
pub(crate) fn is_valid_field(name: &str, c: &Collection) -> bool {
    moul_rules::is_valid_field(name, c)
}

/// `POST /api/rules/test`: evaluates a rule against a sample record. Port
/// of the Go engine's rule tester; the duration is not measured.
pub(crate) fn test_endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let body = match cx.req.json_object() {
        Ok(b) => b.unwrap_or_default(),
        Err(()) => {
            return Ok(Response::json(
                400,
                json!({"error": "Invalid request payload"}),
            ));
        }
    };
    let rule = body
        .get("rule")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let obj = |k: &str| body.get(k).and_then(Value::as_object).cloned();
    let record = obj("record").unwrap_or_default();
    let auth = obj("auth");
    let request = obj("request");
    let translated = match moul_rules::translate(&rule) {
        Ok(t) => t,
        Err(e) => {
            return Ok(Response::json(
                200,
                json!({"valid": false, "matched": false, "translated": "", "duration_us": 0,
                       "error": format!("Translation syntax error: {e}")}),
            ));
        }
    };
    Ok(
        match allows(cx, &rule, auth.as_ref(), &record, request.as_ref()) {
            Ok(matched) => Response::json(
                200,
                json!({"valid": true, "matched": matched, "translated": translated, "duration_us": 0}),
            ),
            Err(e) => Response::json(
                200,
                json!({"valid": false, "matched": false, "translated": translated, "duration_us": 0,
                   "error": format!("Evaluation error: {e}")}),
            ),
        },
    )
}
