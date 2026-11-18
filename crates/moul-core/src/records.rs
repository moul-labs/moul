//! Records: list, view, create, update and delete, with access rules,
//! relations, files and realtime events. Port of the Go engine's
//! `RecordHandler`.

use std::collections::BTreeMap;

use moul_store::schema::{Collection, CollectionKind, Field, FieldType, quote};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Response};
use crate::engine::Cx;
use crate::realtime::{Action, RecordEvent};
use crate::values::{self, Record};
use crate::{crypto, files, ids, rules};

/// Params bound by name, `:name` in the SQL.
pub(crate) type Params = Vec<(String, Sql)>;

/// Runs `sql` with named `params` and reads every row as a record. Each row
/// counts against the request's row budget when `cx` meters it.
pub(crate) fn query(
    conn: &Connection,
    sql: &str,
    params: &[(String, Sql)],
    meter: Option<&crate::budget::Meter>,
) -> ApiResult<Vec<Record>> {
    let mut stmt = conn.prepare(sql)?;
    bind(&mut stmt, params)?;
    let columns: Vec<String> = stmt.column_names().map(str::to_owned).collect();
    let mut rows = stmt.raw_query();
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        if let Some(m) = meter {
            m.rows(1)?;
        }
        out.push(values::read_row(row, &columns)?);
    }
    Ok(out)
}

fn bind(stmt: &mut rusqlite::Statement<'_>, params: &[(String, Sql)]) -> ApiResult<()> {
    for (name, value) in params {
        let key = if name.starts_with(':') {
            name.clone()
        } else {
            format!(":{name}")
        };
        if let Some(i) = stmt.parameter_index(&key)? {
            stmt.raw_bind_parameter(i, value)?;
        }
    }
    Ok(())
}

/// Counts rows with `sql` (a `SELECT COUNT(1)`) and named `params`.
fn count(conn: &Connection, sql: &str, params: &[(String, Sql)]) -> ApiResult<i64> {
    let mut stmt = conn.prepare(sql)?;
    bind(&mut stmt, params)?;
    let mut rows = stmt.raw_query();
    Ok(match rows.next()? {
        Some(r) => r.get(0)?,
        None => 0,
    })
}

/// The stored row of record `id`, as JSON without field types applied.
pub(crate) fn load(conn: &Connection, c: &Collection, id: &str) -> ApiResult<Option<Record>> {
    Ok(query(
        conn,
        &format!("SELECT * FROM {} WHERE id = :id", quote(&c.name)),
        &[("id".into(), Sql::Text(id.into()))],
        None,
    )?
    .into_iter()
    .next())
}

/// Record `id` with field types applied.
pub(crate) fn load_normalized(cx: &Cx<'_>, c: &Collection, id: &str) -> ApiResult<Option<Record>> {
    Ok(load(cx.conn, c, id)?.map(|mut r| {
        values::normalize(c, &mut r);
        r
    }))
}

fn record_exists(conn: &Connection, table: &str, id: &str) -> ApiResult<bool> {
    Ok(conn
        .query_row(
            &format!("SELECT 1 FROM {} WHERE id = ?1", quote(table)),
            [id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn denied(cx: &Cx<'_>) -> ApiError {
    if cx.who.auth_map().is_none() {
        ApiError::unauthorized("Authentication required to perform this action")
    } else {
        ApiError::forbidden("You are not allowed to perform this action")
    }
}

/// The single answer for a missing record and for one the caller may not
/// view. A rejected token gets 401, so clients notice an expired session.
fn record_not_found(cx: &Cx<'_>) -> ApiError {
    if cx.who.rejected() && !cx.who.is_admin() {
        return ApiError::unauthorized("Authentication required to view this record");
    }
    ApiError::not_found("Record not found")
}

fn collection_for_records(cx: &mut Cx<'_>, name: &str) -> ApiResult<Collection> {
    if name.starts_with('_') {
        return Err(ApiError::not_found("Collection not found"));
    }
    cx.collection(name)?
        .ok_or_else(|| ApiError::not_found("Not found"))
}

/// Whether the caller passes `c`'s view rule for `record`. A rule that
/// fails to evaluate hides the record.
pub(crate) fn can_view(cx: &mut Cx<'_>, c: &Collection, record: &Record) -> bool {
    let auth = cx.who.auth_map();
    let ctx = cx.request_context(None);
    rules::allows(cx, &c.rules.view_rule, auth.as_ref(), record, Some(&ctx)).unwrap_or(false)
}

/// Removes what the viewer may not see: an auth record's email unless the
/// viewer is an admin, is that person, or the record opted in through an
/// `emailVisibility` field. Port of Go's `RedactForViewer`.
pub(crate) fn redact(
    c: &Collection,
    rec: &mut Record,
    viewer: Option<&Map<String, Value>>,
    admin: bool,
) {
    if c.kind != CollectionKind::Auth || admin {
        return;
    }
    let is_self = viewer.is_some_and(|v| {
        let vid = v.get("id").and_then(Value::as_str).unwrap_or("");
        !vid.is_empty()
            && rec.get("id").and_then(Value::as_str) == Some(vid)
            && v.get("moul").and_then(Value::as_str) == Some(c.name.as_str())
    });
    let published = c.field("emailVisibility").is_some()
        && match rec.get("emailVisibility") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
            Some(Value::Number(n)) => n.as_f64() == Some(1.0),
            _ => false,
        };
    if !is_self && !published {
        rec.remove("email");
    }
}

/// Finishes a record for a response: signs file URLs, expands requested
/// relations under the caller's view rules, and redacts.
pub(crate) fn present(cx: &mut Cx<'_>, c: &Collection, mut rec: Record) -> Record {
    files::sign_record(cx, c, &mut rec);
    let expand = cx.req.query_param("expand").to_owned();
    if !expand.is_empty() {
        expand_relations(cx, c, &mut rec, &expand);
    }
    let viewer = cx.who.auth_map();
    redact(c, &mut rec, viewer.as_ref(), cx.who.is_admin());
    rec
}

fn expand_relations(cx: &mut Cx<'_>, c: &Collection, rec: &mut Record, expand: &str) {
    let admin = cx.who.is_admin();
    let viewer = cx.who.auth_map();
    let mut out = Map::new();
    for name in expand.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some(field) = c
            .fields
            .iter()
            .find(|f| f.name == name && f.kind == FieldType::Relation && f.relation.is_some())
        else {
            continue;
        };
        let rel = field.relation.as_ref().expect("checked above");
        let Ok(Some(target)) = cx.collection(&rel.target_moul) else {
            continue;
        };
        let load_visible = |cx: &mut Cx<'_>, id: &str| -> Option<Record> {
            let mut t = load_normalized(cx, &target, id).ok().flatten()?;
            if !admin && !can_view(cx, &target, &t) {
                return None;
            }
            files::sign_record(cx, &target, &mut t);
            redact(&target, &mut t, viewer.as_ref(), admin);
            Some(t)
        };
        if rel.is_many() {
            let ids: Vec<String> = rec
                .get(name)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let found: Vec<Value> = ids
                .iter()
                .filter_map(|id| load_visible(cx, id).map(Value::Object))
                .collect();
            out.insert(name.to_owned(), Value::Array(found));
        } else {
            let v = match rec.get(name).and_then(Value::as_str) {
                Some(id) if !id.is_empty() => load_visible(cx, id)
                    .map(Value::Object)
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            };
            out.insert(name.to_owned(), v);
        }
    }
    if !out.is_empty() {
        rec.insert("expand".into(), Value::Object(out));
    }
}

/// Port of Go's `parseUniqueConstraintError`.
/// Port of Go's `parseUniqueConstraintError`, read from SQLite's own error
/// before it is turned into a generic answer. A budget interruption keeps
/// its meaning.
fn write_error(e: rusqlite::Error, c: &Collection, fallback: &str) -> ApiError {
    let text = e.to_string();
    let api = ApiError::from(e);
    if api.extra.contains_key("budget") {
        return api;
    }
    unique_error(&text, c).unwrap_or_else(|| ApiError::new(500, fallback))
}

fn unique_error(message: &str, c: &Collection) -> Option<ApiError> {
    let idx = message.find("UNIQUE constraint failed: ")?;
    let details = message[idx + "UNIQUE constraint failed: ".len()..].trim();
    let first = details.split(',').next().unwrap_or("").trim();
    let col = first.rsplit('.').next().unwrap_or(first).trim();
    if c.kind == CollectionKind::Auth && (col == "username" || col == "email") {
        return Some(ApiError::bad_request("Username or Email already exists"));
    }
    if !col.is_empty() {
        return Some(ApiError::bad_request(format!(
            "Value for field {col:?} already exists"
        )));
    }
    Some(ApiError::bad_request("Unique constraint violation"))
}

fn field_key(f: &Field) -> String {
    if f.id.is_empty() {
        f.name.clone()
    } else {
        f.id.clone()
    }
}

fn to_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// What a create or update writes, with the checks that must wait until
/// the caller has passed the rule.
struct Writes {
    values: BTreeMap<String, Sql>,
    /// Relation targets to check: target collection, id, and for a 1:1
    /// field the field name that must not already point at it.
    relation_checks: Vec<(String, String, Option<String>)>,
    /// Field ids or names changed.
    changed: Vec<String>,
}

/// Converts the fields of `body` that `c` defines into stored values.
fn field_writes(
    cx: &mut Cx<'_>,
    c: &Collection,
    body: &Map<String, Value>,
    record_id: &str,
    is_create: bool,
) -> ApiResult<Writes> {
    let mut w = Writes {
        values: BTreeMap::new(),
        relation_checks: Vec::new(),
        changed: Vec::new(),
    };
    for f in &c.fields {
        let val = body.get(&f.name);
        if is_create {
            values::check_constraints(f, val)?;
        } else if let Some(v) = val {
            values::check_constraints(f, Some(v))?;
        }
        let Some(val) = val else { continue };
        let stored = match f.kind {
            FieldType::File => match files::stored_value(cx, c, f, val, record_id)? {
                Some(s) => Sql::Text(s),
                None => Sql::Null,
            },
            FieldType::Relation => {
                let many = f.relation.as_ref().is_some_and(|r| r.is_many());
                if val.is_null() || val.as_str() == Some("") {
                    Sql::Text(if many { "[]".into() } else { String::new() })
                } else {
                    let rel = f.relation.as_ref().ok_or_else(|| {
                        ApiError::bad_request(format!(
                            "relation field {:?} requires relationConfig",
                            f.name
                        ))
                    })?;
                    let ids = values::relation_ids(f, val)?;
                    if many {
                        for id in &ids {
                            w.relation_checks
                                .push((rel.target_moul.clone(), id.clone(), None));
                        }
                        Sql::Text(serde_json::to_string(&ids).expect("ids serialise"))
                    } else {
                        let id = ids.into_iter().next().unwrap_or_default();
                        if !id.is_empty() {
                            let unique = (rel.cardinality == "1:1").then(|| f.name.clone());
                            w.relation_checks
                                .push((rel.target_moul.clone(), id.clone(), unique));
                        }
                        Sql::Text(id)
                    }
                }
            }
            _ => values::to_sql(f, val)?,
        };
        w.values.insert(f.name.clone(), stored);
        w.changed.push(field_key(f));
    }
    Ok(w)
}

fn check_relations(
    cx: &Cx<'_>,
    c: &Collection,
    w: &Writes,
    record_id: Option<&str>,
) -> ApiResult<()> {
    for (target, id, unique_field) in &w.relation_checks {
        if !record_exists(cx.conn, target, id).unwrap_or(false) {
            return Err(ApiError::bad_request(format!(
                "Reference record {id} in collection {target} does not exist"
            )));
        }
        if let Some(field) = unique_field {
            let mut sql = format!(
                "SELECT COUNT(1) FROM {} WHERE {} = :val",
                quote(&c.name),
                quote(field)
            );
            let mut params: Params = vec![("val".into(), Sql::Text(id.clone()))];
            if let Some(rid) = record_id {
                sql += " AND id != :id";
                params.push(("id".into(), Sql::Text(rid.into())));
            }
            if count(cx.conn, &sql, &params)? > 0 {
                return Err(ApiError::bad_request(format!(
                    "1:1 relation field {field} already references record {id}"
                )));
            }
        }
    }
    Ok(())
}

fn body(cx: &Cx<'_>) -> ApiResult<Map<String, Value>> {
    match cx.req.json_object() {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Ok(Map::new()),
        Err(()) => Err(ApiError::bad_request("Invalid JSON payload")),
    }
}

fn sql_as_json(v: &Sql) -> Value {
    match v {
        Sql::Null => Value::Null,
        Sql::Integer(i) => json!(i),
        Sql::Real(f) => values::number(*f),
        Sql::Text(s) => json!(s),
        Sql::Blob(b) => json!(crypto::base64(b)),
    }
}

/// `POST /api/moul/:name/records`
pub(crate) fn create(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = collection_for_records(cx, name)?;
    let body = body(cx)?;
    if c.kind == CollectionKind::Analytic {
        return Err(ApiError::bad_request(
            "analytic collections are not supported by this engine yet",
        ));
    }
    let id = match body.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => {
            if id.len() > 128 || id.chars().any(|ch| ch.is_control() || ch == '/') {
                return Err(ApiError::bad_request(
                    "id must be at most 128 characters and contain no '/'",
                ));
            }
            // A URL resolves these as path steps: `/records/..` is the collection
            // itself, so a request for such a record would act on the collection.
            if id == "." || id == ".." {
                return Err(ApiError::bad_request("id must not be '.' or '..'"));
            }
            id.to_owned()
        }
        _ => ids::record_id(cx.host, &c.name),
    };
    let mut w = field_writes(cx, &c, &body, "", true)?;
    let now = cx.now();
    w.values.insert("id".into(), Sql::Text(id.clone()));
    w.values.insert("createdAt".into(), Sql::Text(now.clone()));
    w.values.insert("updatedAt".into(), Sql::Text(now.clone()));

    let mut password_hash = None;
    if c.kind == CollectionKind::Auth {
        let username = body.get("username").and_then(Value::as_str).unwrap_or("");
        let email = body.get("email").and_then(Value::as_str).unwrap_or("");
        let password = body.get("password").and_then(Value::as_str).unwrap_or("");
        let confirm = body
            .get("passwordConfirm")
            .and_then(Value::as_str)
            .unwrap_or("");
        if username.is_empty() || email.is_empty() {
            return Err(ApiError::bad_request(
                "username and email are required for auth mouls",
            ));
        }
        w.values
            .insert("username".into(), Sql::Text(username.into()));
        w.values.insert("email".into(), Sql::Text(email.into()));
        w.changed
            .extend(["username".to_owned(), "email".to_owned()]);
        if !password.is_empty() || !confirm.is_empty() {
            if password != confirm {
                return Err(ApiError::bad_request(
                    "password and passwordConfirm must match",
                ));
            }
            crypto::validate_password(password).map_err(ApiError::bad_request)?;
            password_hash = Some(password.to_owned());
        }
    }
    if c.kind == CollectionKind::Worker {
        worker_create_values(&body, &now, &mut w)?;
    }

    // As in the Go engine, the create rule sees the values about to be
    // inserted: booleans as 0 or 1, JSON, files and lists as text.
    let rule_record: Record = w
        .values
        .iter()
        .map(|(k, v)| (k.clone(), sql_as_json(v)))
        .collect();
    if !cx.who.is_admin() {
        let auth = cx.who.auth_map();
        let ctx = cx.request_context(Some(&body));
        match rules::allows(
            cx,
            &c.rules.create_rule,
            auth.as_ref(),
            &rule_record,
            Some(&ctx),
        ) {
            Ok(true) => {}
            Ok(false) => return Err(denied(cx)),
            Err(_) => return Err(ApiError::internal()),
        }
    }
    // Relation targets are checked only after the rule passed, so a caller
    // who may not create cannot probe for records.
    check_relations(cx, &c, &w, None)?;

    let cols: Vec<String> = w.values.keys().map(|k| quote(k)).collect();
    let marks: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote(&c.name),
        cols.join(", "),
        marks.join(", ")
    );
    if let Err(e) = cx
        .conn
        .execute(&sql, rusqlite::params_from_iter(w.values.values()))
    {
        return Err(write_error(e, &c, "Failed to insert record"));
    }
    if let Some(password) = password_hash {
        let hash = crypto::hash_password(cx.host, &password);
        crate::auth::set_secret(cx.conn, &c.id, &id, "passwordHash", Some(&hash))?;
    }

    let mut rec = load_normalized(cx, &c, &id)?.ok_or_else(ApiError::internal)?;
    files::sync_refs(cx, &c, &rec)?;
    cx.events.push(RecordEvent {
        action: Action::Create,
        collection: c.clone(),
        record: rec.clone(),
        old: None,
        from_log: false,
    });
    cx.changed(&c, Some(&id), w.changed);
    rec = present(cx, &c, rec);
    Ok(Response::json(201, Value::Object(rec)))
}

fn worker_create_values(body: &Map<String, Value>, now: &str, w: &mut Writes) -> ApiResult<()> {
    let worker = body.get("worker").and_then(Value::as_str).unwrap_or("");
    if worker.is_empty() {
        return Err(ApiError::bad_request(
            "worker name is required for worker mouls",
        ));
    }
    let queue = body
        .get("queue")
        .and_then(Value::as_str)
        .filter(|q| !q.is_empty())
        .unwrap_or("default");
    let scheduled = match body.get("scheduled_at").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => {
            if crate::time::parse_rfc3339(s).is_none() {
                return Err(ApiError::bad_request(
                    "Invalid scheduled_at format (must be RFC3339)",
                ));
            }
            s.to_owned()
        }
        _ => now.to_owned(),
    };
    let v = &mut w.values;
    v.insert("worker".into(), Sql::Text(worker.into()));
    v.insert("queue".into(), Sql::Text(queue.into()));
    v.insert("state".into(), Sql::Text("available".into()));
    v.insert("attempt".into(), Sql::Integer(0));
    v.insert("errors".into(), Sql::Text("[]".into()));
    v.insert(
        "max_attempts".into(),
        Sql::Integer(body.get("max_attempts").and_then(to_int).unwrap_or(20)),
    );
    v.insert(
        "priority".into(),
        Sql::Integer(body.get("priority").and_then(to_int).unwrap_or(0)),
    );
    v.insert("inserted_at".into(), Sql::Text(now.into()));
    v.insert("scheduled_at".into(), Sql::Text(scheduled));
    for (key, default) in [("args", "{}"), ("meta", "{}"), ("tags", "[]")] {
        let s = match body.get(key) {
            Some(val) => serde_json::to_string(val).expect("json serialises"),
            None => default.to_owned(),
        };
        v.insert(key.into(), Sql::Text(s));
    }
    w.changed.push("worker".into());
    Ok(())
}

/// `GET /api/moul/:name/records/:id`
pub(crate) fn view(cx: &mut Cx<'_>, name: &str, id: &str) -> ApiResult {
    let c = collection_for_records(cx, name)?;
    let Some(rec) = load_normalized(cx, &c, id)? else {
        return Err(record_not_found(cx));
    };
    cx.meter.rows(1)?;
    if !cx.who.is_admin() && !can_view(cx, &c, &rec) {
        return Err(record_not_found(cx));
    }
    let rec = present(cx, &c, rec);
    Ok(Response::json(200, Value::Object(rec)))
}

/// The columns an update rule pins to the caller (`owner = @request.auth.id`)
/// that `body` changes to someone else. Port of Go's `reassignedOwnerField`.
fn reassigned_owner_field(
    c: &Collection,
    body: &Map<String, Value>,
    auth: Option<&Map<String, Value>>,
    stored: &Record,
) -> Option<String> {
    let caller = auth
        .and_then(|a| a.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    for field in rules::owner_fields(&c.rules.update_rule) {
        let Some(val) = body.get(&field) else {
            continue;
        };
        let Some(s) = val.as_str() else {
            return Some(field);
        };
        if stored.get(&field).and_then(Value::as_str) == Some(s) {
            continue;
        }
        if s != caller || caller.is_empty() {
            return Some(field);
        }
    }
    None
}

/// `PATCH /api/moul/:name/records/:id`
pub(crate) fn update(cx: &mut Cx<'_>, name: &str, id: &str) -> ApiResult {
    let c = collection_for_records(cx, name)?;
    let Some(old) = load_normalized(cx, &c, id)? else {
        return Err(record_not_found(cx));
    };
    let admin = cx.who.is_admin();
    if !admin && !can_view(cx, &c, &old) {
        return Err(record_not_found(cx));
    }
    let body = body(cx)?;
    let auth = cx.who.auth_map();
    if !admin {
        let ctx = cx.request_context(Some(&body));
        match rules::allows(cx, &c.rules.update_rule, auth.as_ref(), &old, Some(&ctx)) {
            Ok(true) => {}
            Ok(false) => return Err(denied(cx)),
            Err(_) => return Err(ApiError::internal()),
        }
        if let Some(field) = reassigned_owner_field(&c, &body, auth.as_ref(), &old) {
            return Err(ApiError::forbidden(format!(
                "You are not allowed to set {field:?} to another user"
            )));
        }
    }
    let mut w = field_writes(cx, &c, &body, id, false)?;
    check_relations(cx, &c, &w, Some(id))?;

    let mut new_password = None;
    if c.kind == CollectionKind::Auth {
        for key in ["username", "email"] {
            if let Some(v) = body
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                w.values.insert(key.into(), Sql::Text(v.into()));
                w.changed.push(key.into());
            }
        }
        if let Some(password) = body
            .get("password")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        {
            let confirm = body
                .get("passwordConfirm")
                .and_then(Value::as_str)
                .unwrap_or("");
            if password != confirm {
                return Err(ApiError::bad_request(
                    "password and passwordConfirm must match",
                ));
            }
            crypto::validate_password(password).map_err(ApiError::bad_request)?;
            new_password = Some(password.to_owned());
        }
    }
    if c.kind == CollectionKind::Worker {
        worker_update_values(&body, &mut w)?;
    }

    if !w.values.is_empty() {
        w.values.insert("updatedAt".into(), Sql::Text(cx.now()));
        let sets: Vec<String> = w
            .values
            .keys()
            .enumerate()
            .map(|(i, k)| format!("{} = ?{}", quote(k), i + 1))
            .collect();
        let sql = format!(
            "UPDATE {} SET {} WHERE id = ?{}",
            quote(&c.name),
            sets.join(", "),
            w.values.len() + 1
        );
        let mut params: Vec<Sql> = w.values.values().cloned().collect();
        params.push(Sql::Text(id.into()));
        if let Err(e) = cx.conn.execute(&sql, rusqlite::params_from_iter(params)) {
            return Err(write_error(e, &c, "Failed to update record"));
        }
    }
    if let Some(password) = new_password {
        let hash = crypto::hash_password(cx.host, &password);
        crate::auth::set_secret(cx.conn, &c.id, id, "passwordHash", Some(&hash))?;
    }

    let rec = load_normalized(cx, &c, id)?.ok_or_else(ApiError::internal)?;
    files::sync_refs(cx, &c, &rec)?;
    if !w.values.is_empty() {
        cx.events.push(RecordEvent {
            action: Action::Update,
            collection: c.clone(),
            record: rec.clone(),
            old: Some(old),
            from_log: false,
        });
        cx.changed(&c, Some(id), w.changed);
    }
    let rec = present(cx, &c, rec);
    Ok(Response::json(200, Value::Object(rec)))
}

fn worker_update_values(body: &Map<String, Value>, w: &mut Writes) -> ApiResult<()> {
    for key in ["state", "queue", "worker"] {
        if let Some(v) = body
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            w.values.insert(key.into(), Sql::Text(v.into()));
            w.changed.push(key.into());
        }
    }
    if let Some(s) = body
        .get("scheduled_at")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        if crate::time::parse_rfc3339(s).is_none() {
            return Err(ApiError::bad_request(
                "Invalid scheduled_at format (must be RFC3339)",
            ));
        }
        w.values.insert("scheduled_at".into(), Sql::Text(s.into()));
        w.changed.push("scheduled_at".into());
    }
    for key in ["attempt", "max_attempts", "priority"] {
        if let Some(n) = body.get(key).and_then(to_int) {
            w.values.insert(key.into(), Sql::Integer(n));
            w.changed.push(key.into());
        }
    }
    for key in ["args", "meta", "tags", "errors"] {
        if let Some(v) = body.get(key) {
            w.values.insert(
                key.into(),
                Sql::Text(serde_json::to_string(v).expect("json serialises")),
            );
            w.changed.push(key.into());
        }
    }
    Ok(())
}

/// `DELETE /api/moul/:name/records/:id`
pub(crate) fn delete(cx: &mut Cx<'_>, name: &str, id: &str) -> ApiResult {
    let c = collection_for_records(cx, name)?;
    let Some(rec) = load_normalized(cx, &c, id)? else {
        return Err(record_not_found(cx));
    };
    let admin = cx.who.is_admin();
    if !admin && !can_view(cx, &c, &rec) {
        return Err(record_not_found(cx));
    }
    if !admin {
        let auth = cx.who.auth_map();
        let ctx = cx.request_context(None);
        match rules::allows(cx, &c.rules.delete_rule, auth.as_ref(), &rec, Some(&ctx)) {
            Ok(true) => {}
            Ok(false) => return Err(denied(cx)),
            Err(_) => return Err(ApiError::internal()),
        }
    }
    delete_cascade(cx, &c, rec, id)?;
    cx.changed(&c, Some(id), Vec::new());
    Ok(Response::empty(204))
}

/// Relation fields of any collection that point at `target`.
fn referencing(cx: &mut Cx<'_>, target: &str) -> ApiResult<Vec<(Collection, Field)>> {
    let mut out = Vec::new();
    for other in cx.collections()? {
        for f in &other.fields {
            if f.kind == FieldType::Relation
                && f.relation.as_ref().is_some_and(|r| r.target_moul == target)
            {
                out.push((other.clone(), f.clone()));
            }
        }
    }
    Ok(out)
}

/// Ids of records in `other` whose relation field `f` names `id`.
fn referrers(cx: &Cx<'_>, other: &Collection, f: &Field, id: &str) -> ApiResult<Vec<String>> {
    let many = f.relation.as_ref().is_some_and(|r| r.is_many());
    let rows = if many {
        query(
            cx.conn,
            &format!(
                "SELECT id, {col} AS v FROM {t} WHERE {col} LIKE :pat",
                col = quote(&f.name),
                t = quote(&other.name)
            ),
            &[("pat".into(), Sql::Text(format!("%{id}%")))],
            None,
        )?
        .into_iter()
        .filter(|r| {
            r.get("v")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                .is_some_and(|ids| ids.iter().any(|x| x == id))
        })
        .collect()
    } else {
        query(
            cx.conn,
            &format!(
                "SELECT id FROM {} WHERE {} = :id",
                quote(&other.name),
                quote(&f.name)
            ),
            &[("id".into(), Sql::Text(id.into()))],
            None,
        )?
    };
    Ok(rows
        .into_iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect())
}

/// Deletes a record and everything its `CASCADE` relations reach, refusing
/// the whole delete when a `RESTRICT` relation from a record that would
/// survive points into that set, then clears `SET_NULL` references from
/// surviving records. Port of Go's `deleteRecordAndCascade`, which deleted
/// depth first: collecting the set first gives the same answer without
/// recursion, so a long chain cannot exhaust the stack, and every cascaded
/// record counts against the row budget. Where Go logged a failed cascade
/// and went on, this engine refuses, so no dangling reference is left.
pub(crate) fn delete_cascade(
    cx: &mut Cx<'_>,
    c: &Collection,
    rec: Record,
    id: &str,
) -> ApiResult<()> {
    let mut refs_of: std::collections::HashMap<String, Vec<(Collection, Field)>> =
        Default::default();
    let mut refs = |cx: &mut Cx<'_>, name: &str| -> ApiResult<Vec<(Collection, Field)>> {
        if let Some(r) = refs_of.get(name) {
            return Ok(r.clone());
        }
        let r = referencing(cx, name)?;
        refs_of.insert(name.to_owned(), r.clone());
        Ok(r)
    };
    let on_delete = |f: &Field| {
        let d = f
            .relation
            .as_ref()
            .map(|r| r.on_delete.as_str())
            .unwrap_or("");
        if d.is_empty() {
            "SET_NULL".to_owned()
        } else {
            d.to_owned()
        }
    };

    // 1. The set the delete removes.
    let mut order: Vec<(Collection, Record, String)> = Vec::new();
    let mut members = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::from([(c.clone(), rec, id.to_owned())]);
    while let Some((c, rec, id)) = queue.pop_front() {
        if !members.insert((c.name.clone(), id.clone())) {
            continue;
        }
        if !order.is_empty() {
            cx.meter.rows(1)?;
        }
        for (other, f) in refs(cx, &c.name)? {
            if on_delete(&f) == "CASCADE" {
                for child in referrers(cx, &other, &f, &id)? {
                    if let Some(child_rec) = load_normalized(cx, &other, &child)? {
                        queue.push_back((other.clone(), child_rec, child));
                    }
                }
            }
        }
        order.push((c, rec, id));
    }

    // 2. A surviving record that RESTRICTs any member blocks the delete.
    for (c, _, id) in &order {
        for (other, f) in refs(cx, &c.name)? {
            if on_delete(&f) != "RESTRICT" {
                continue;
            }
            for r in referrers(cx, &other, &f, id)? {
                if !members.contains(&(other.name.clone(), r)) {
                    return Err(ApiError::bad_request(format!(
                        "cannot delete record {id} in {}: referenced by {}.{} (RESTRICT constraint)",
                        c.name, other.name, f.name
                    )));
                }
            }
        }
    }

    // 3. Delete, and clear references from records that stay.
    for (n, (c, rec, id)) in order.into_iter().enumerate() {
        let r#ref = refs(cx, &c.name)?;
        cx.conn.execute(
            &format!("DELETE FROM {} WHERE id = ?1", quote(&c.name)),
            [&id],
        )?;
        cx.conn.execute(
            "DELETE FROM main._file_refs WHERE moul = ?1 AND recordId = ?2",
            [&c.name, &id],
        )?;
        if c.kind == CollectionKind::Auth {
            cx.conn.execute(
                "DELETE FROM writer._auth WHERE collection = ?1 AND record = ?2",
                [&c.id, &id],
            )?;
        }
        if n > 0 {
            cx.effects.push((c.id.clone(), id.clone(), "delete".into()));
        }
        cx.events.push(RecordEvent {
            action: Action::Delete,
            collection: c.clone(),
            record: rec.clone(),
            old: Some(rec),
            from_log: false,
        });
        for (other, f) in r#ref {
            if on_delete(&f) != "SET_NULL" {
                continue;
            }
            let many = f.relation.as_ref().is_some_and(|r| r.is_many());
            for child in referrers(cx, &other, &f, &id)? {
                if members.contains(&(other.name.clone(), child.clone())) {
                    continue;
                }
                let before = load_normalized(cx, &other, &child)?;
                let value = if many {
                    let Some(row) = load(cx.conn, &other, &child)? else {
                        continue;
                    };
                    let ids: Vec<String> = row
                        .get(&f.name)
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_default();
                    let kept: Vec<String> = ids.into_iter().filter(|x| *x != id).collect();
                    serde_json::to_string(&kept).expect("ids serialise")
                } else {
                    String::new()
                };
                cx.conn.execute(
                    &format!(
                        "UPDATE {} SET {} = ?1 WHERE id = ?2",
                        quote(&other.name),
                        quote(&f.name)
                    ),
                    rusqlite::params![value, child],
                )?;
                // Live subscribers see the cleared reference (Go sent
                // nothing for it).
                if let Some(after) = load_normalized(cx, &other, &child)? {
                    cx.events.push(RecordEvent {
                        action: Action::Update,
                        collection: other.clone(),
                        record: after,
                        old: before,
                        from_log: false,
                    });
                }
                cx.effects.push((other.id.clone(), child, "update".into()));
            }
        }
    }
    Ok(())
}

/// A sort term as `(column, descending)` when it orders by a plain column.
/// Relation paths (subqueries) and `RANDOM()` give `None`.
fn plain_term(term: &str) -> Option<(String, bool)> {
    let mut parts = term.split_whitespace();
    let col = parts.next()?.trim_matches('`');
    let desc = match parts.next() {
        None => false,
        Some(d) if d.eq_ignore_ascii_case("DESC") => true,
        Some(d) if d.eq_ignore_ascii_case("ASC") => false,
        Some(_) => return None,
    };
    let plain = !col.is_empty()
        && col.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && parts.next().is_none();
    plain.then(|| (col.to_owned(), desc))
}

/// A sort term with its column quoted, as the Go engine's database layer
/// wrote it: a field named `order` or `group` sorts like any other.
pub(crate) fn quoted_term(term: &str) -> String {
    match plain_term(term) {
        Some((col, desc)) => format!("{} {}", quote(&col), if desc { "DESC" } else { "ASC" }),
        None => term.to_owned(),
    }
}

/// Builds the cursor condition for `?after=<id>`, reading the cursor record
/// under the caller's list rule: rows strictly after the cursor in the
/// full sort order, which always ends with `id`. Port of Go's
/// `buildCursorWhere`, made exact for ties.
fn cursor_where(
    cx: &Cx<'_>,
    c: &Collection,
    sort: &[String],
    after: &str,
    rule: Option<&(String, Params)>,
) -> ApiResult<(String, Params)> {
    let simple = |op: &str| {
        (
            format!("id {op} :cursor_after"),
            vec![("cursor_after".into(), Sql::Text(after.into()))],
        )
    };
    let terms: Option<Vec<(String, bool)>> = sort.iter().map(|t| plain_term(t)).collect();
    let Some(terms) = terms else {
        // Relation paths and random order have no position to resume from.
        return Ok(simple(">"));
    };
    match terms.as_slice() {
        [] => return Ok(simple(">")),
        [(col, desc)] if col == "id" => return Ok(simple(if *desc { "<" } else { ">" })),
        _ => {}
    }
    let mut sql = format!(
        "SELECT * FROM {} WHERE id = :cursor_id_lookup",
        quote(&c.name)
    );
    let mut params: Params = vec![("cursor_id_lookup".into(), Sql::Text(after.into()))];
    if let Some((rsql, rparams)) = rule {
        sql += &format!(" AND ({rsql})");
        params.extend(rparams.iter().cloned());
    }
    let Some(cursor) = query(cx.conn, &sql, &params, None)?.into_iter().next() else {
        return Ok(simple(">"));
    };
    // (t1 after v1) OR (t1 same v1 AND t2 after v2) OR ..., where "after"
    // and "same" follow SQLite's order: NULL sorts first ascending and
    // last descending. The terms end with id, so the chain always ends.
    let mut ors = Vec::new();
    let mut eqs: Vec<String> = Vec::new();
    let mut out: Params = Vec::new();
    for (i, (col, desc)) in terms.iter().enumerate() {
        let q = quote(col);
        let name = format!("cursor_v{i}");
        let val = cursor.get(col).cloned().unwrap_or(Value::Null);
        let (after, same) = if val.is_null() {
            let after = if *desc {
                None
            } else {
                Some(format!("{q} IS NOT NULL"))
            };
            (after, format!("{q} IS NULL"))
        } else {
            out.push((name.clone(), values::json_to_sql(&val)));
            let after = if *desc {
                format!("({q} < :{name} OR {q} IS NULL)")
            } else {
                format!("{q} > :{name}")
            };
            (Some(after), format!("{q} = :{name}"))
        };
        if let Some(after) = after {
            let mut clause = eqs.clone();
            clause.push(after);
            ors.push(format!("({})", clause.join(" AND ")));
        }
        eqs.push(same);
    }
    if ors.is_empty() {
        return Ok(simple(">"));
    }
    Ok((ors.join(" OR "), out))
}

fn to_params(p: Vec<(String, Value)>) -> Params {
    p.into_iter()
        .map(|(k, v)| (k, values::json_to_sql(&v)))
        .collect()
}

/// `GET /api/moul/:name/records`
pub(crate) fn list(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = collection_for_records(cx, name)?;
    let admin = cx.who.is_admin();
    let auth = cx.who.auth_map();
    let ctx = cx.request_context(None);

    let page = cx
        .req
        .query_param("page")
        .parse::<i64>()
        .ok()
        .filter(|p| *p > 0)
        .unwrap_or(1);
    let per_page = cx
        .req
        .query_param("perPage")
        .parse::<i64>()
        .ok()
        .filter(|p| *p > 0)
        .unwrap_or(30)
        .min(500);

    // The list rule and the caller's filter bind under separate prefixes,
    // so a filter's values can never replace the rule's.
    let rule = if admin || c.rules.list_rule.trim().is_empty() {
        None
    } else {
        match rules::filter(
            cx,
            &c,
            &c.rules.list_rule,
            auth.as_ref(),
            Some(&ctx),
            "rule_",
            false,
        ) {
            Ok(Some((sql, p))) if !sql.trim().is_empty() => Some((sql, to_params(p))),
            Ok(_) => None,
            Err(_) => return Err(ApiError::internal()),
        }
    };
    let filter_param = cx.req.query_param("filter").to_owned();
    let filter = if filter_param.is_empty() {
        None
    } else {
        match rules::filter(
            cx,
            &c,
            &filter_param,
            auth.as_ref(),
            Some(&ctx),
            "filter_",
            !admin,
        ) {
            Ok(Some((sql, p))) if !sql.trim().is_empty() => Some((sql, to_params(p))),
            Ok(_) => None,
            Err(e) => {
                return Err(ApiError::bad_request(format!(
                    "Invalid filter parameter: {e}"
                )));
            }
        }
    };

    let mut wheres = Vec::new();
    let mut params: Params = Vec::new();
    if let Some((sql, p)) = &rule {
        wheres.push(format!("({sql})"));
        params.extend(p.iter().cloned());
    }
    if let Some((sql, p)) = &filter {
        wheres.push(format!("({sql})"));
        params.extend(p.iter().cloned());
    }
    let search = cx.req.query_param("search").trim().to_owned();
    if !search.is_empty() {
        let mut conds = vec!["id LIKE :search_term".to_owned()];
        match c.kind {
            CollectionKind::Auth => {
                conds.push("username LIKE :search_term".into());
                if admin {
                    conds.push("email LIKE :search_term".into());
                }
            }
            CollectionKind::Worker => {
                conds.extend([
                    "worker LIKE :search_term".to_owned(),
                    "queue LIKE :search_term".to_owned(),
                    "state LIKE :search_term".to_owned(),
                ]);
            }
            _ => {}
        }
        for f in &c.fields {
            if matches!(f.kind, FieldType::Text | FieldType::Email | FieldType::Url)
                && (admin || !rules::is_secret_field(&f.name, &c))
            {
                conds.push(format!("{} LIKE :search_term", quote(&f.name)));
            }
        }
        wheres.push(format!("({})", conds.join(" OR ")));
        params.push(("search_term".into(), Sql::Text(format!("%{search}%"))));
    }

    let sort_param = cx.req.query_param("sort").to_owned();
    let mut sort = rules::sort(cx, &c, &sort_param, !admin)
        .map_err(|e| ApiError::bad_request(format!("Invalid sort parameter: {e}")))?;
    if sort.is_empty() && rules::is_valid_field("id", &c) {
        sort = vec!["id ASC".into()];
    }
    // Ties are broken by id, so every page, and every cursor, sees one
    // order.
    let has_id = sort
        .iter()
        .any(|t| plain_term(t).is_some_and(|(col, _)| col == "id"));
    let random = sort.iter().any(|t| t.eq_ignore_ascii_case("RANDOM()"));
    if !has_id && !random && rules::is_valid_field("id", &c) {
        sort.push("id ASC".into());
    }

    let table = quote(&c.name);
    let where_sql = if wheres.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wheres.join(" AND "))
    };
    // The rule and filter SQL runs fenced into the shared store.
    let total = cx.guard.run(cx.conn, || {
        count(
            cx.conn,
            &format!("SELECT COUNT(1) FROM {table}{where_sql}"),
            &params,
        )
    })??;
    let total_pages = if total > 0 {
        (total + per_page - 1) / per_page
    } else {
        0
    };

    let mut data_wheres = wheres.clone();
    let mut data_params = params.clone();
    let after = cx.req.query_param("after").to_owned();
    if !after.is_empty() {
        let (csql, cparams) = cx.guard.run(cx.conn, || {
            cursor_where(cx, &c, &sort, &after, rule.as_ref())
        })??;
        data_wheres.push(format!("({csql})"));
        data_params.extend(cparams);
    }
    let mut sql = format!("SELECT * FROM {table}");
    if !data_wheres.is_empty() {
        sql += &format!(" WHERE {}", data_wheres.join(" AND "));
    }
    if !sort.is_empty() {
        let terms: Vec<String> = sort.iter().map(|t| quoted_term(t)).collect();
        sql += &format!(" ORDER BY {}", terms.join(", "));
    }
    if after.is_empty() {
        // As in Go, a huge page number wraps; SQLite reads a negative
        // offset as zero.
        let offset = (page - 1).wrapping_mul(per_page);
        sql += &format!(" LIMIT {per_page} OFFSET {offset}");
    } else {
        sql += &format!(" LIMIT {per_page}");
    }
    let rows = cx.guard.run(cx.conn, || {
        query(cx.conn, &sql, &data_params, Some(cx.meter))
    })??;
    let mut items = Vec::with_capacity(rows.len());
    for mut rec in rows {
        values::normalize(&c, &mut rec);
        items.push(Value::Object(present(cx, &c, rec)));
    }
    Ok(Response::json(
        200,
        json!({
            "page": page,
            "perPage": per_page,
            "totalItems": total,
            "totalPages": total_pages,
            "items": items,
        }),
    ))
}
