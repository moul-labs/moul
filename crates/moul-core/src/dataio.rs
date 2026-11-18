//! Import and export of a collection's records as CSV or JSON. Port of the
//! Go engine's `dataio` package and its handlers.
//!
//! One change from Go: credentials never appear in an export. An import may
//! still carry `passwordHash`, which goes to the writer-only store, so users
//! can move between databases.

use moul_store::schema::{Collection, CollectionKind, FieldType, quote};
use rusqlite::types::Value as Sql;
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Body, Response};
use crate::engine::Cx;
use crate::{auth, files, ids, records, rules, time, values};

fn collection(cx: &mut Cx<'_>, name: &str) -> ApiResult<Collection> {
    cx.collection(name)?
        .ok_or_else(|| ApiError::not_found(format!("Collection {name:?} not found")))
}

/// The CSV columns of an export, in Go's order, without credentials.
pub fn column_names(c: &Collection) -> Vec<String> {
    let mut cols = vec!["id".to_owned()];
    match c.kind {
        CollectionKind::Auth => cols.extend(["username".into(), "email".into()]),
        CollectionKind::Worker => cols.extend(
            [
                "worker",
                "state",
                "queue",
                "priority",
                "attempt",
                "max_attempts",
            ]
            .map(String::from),
        ),
        CollectionKind::Analytic => cols.extend(
            [
                "name",
                "properties",
                "visit_token",
                "visitor_token",
                "user_id",
            ]
            .map(String::from),
        ),
        CollectionKind::Base => {}
    }
    cols.extend(c.fields.iter().map(|f| f.name.clone()));
    cols.extend(["createdAt".into(), "updatedAt".into()]);
    cols
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// Writes records as CSV with a header row, as Go's `encoding/csv` does.
pub fn to_csv(records: &[Map<String, Value>], headers: &[String]) -> String {
    let mut out = String::new();
    out += &headers
        .iter()
        .map(|h| csv_cell(h))
        .collect::<Vec<_>>()
        .join(",");
    out.push('\n');
    for rec in records {
        let row: Vec<String> = headers
            .iter()
            .map(|h| match rec.get(h) {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => csv_cell(s),
                Some(Value::Bool(b)) => b.to_string(),
                Some(Value::Number(n)) => n.to_string(),
                Some(other) => csv_cell(&other.to_string()),
            })
            .collect();
        out += &row.join(",");
        out.push('\n');
    }
    out
}

/// Splits CSV text into rows of fields (RFC 4180, with quoted fields).
pub fn parse_csv_rows(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut at_start = true;
    let mut chars = text.chars().peekable();
    let mut line = 1;
    while let Some(ch) = chars.next() {
        if quoted {
            match ch {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                '\n' => {
                    line += 1;
                    field.push(ch);
                }
                _ => field.push(ch),
            }
            continue;
        }
        match ch {
            '"' if at_start => {
                quoted = true;
                at_start = false;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                at_start = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                at_start = true;
                line += 1;
            }
            ' ' | '\t' if at_start => {}
            _ => {
                field.push(ch);
                at_start = false;
            }
        }
    }
    if quoted {
        return Err(format!(
            "error reading csv line {line}: extraneous or missing \" in quoted-field"
        ));
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

/// Parses CSV into records, typing values by the collection's fields.
pub fn parse_csv(text: &str, c: &Collection) -> Result<Vec<Map<String, Value>>, String> {
    let mut rows = parse_csv_rows(text)?.into_iter();
    let Some(headers) = rows.next() else {
        return Ok(vec![]);
    };
    let headers: Vec<String> = headers.into_iter().map(|h| h.trim().to_owned()).collect();
    let mut out = Vec::new();
    for row in rows {
        let mut rec = Map::new();
        let mut has_values = false;
        for (i, raw) in row.iter().enumerate() {
            let Some(header) = headers.get(i).filter(|h| !h.is_empty()) else {
                continue;
            };
            let v = raw.trim();
            if !v.is_empty() {
                has_values = true;
            }
            let kind = c.field(header).map(|f| f.kind.clone());
            let value = match kind {
                Some(FieldType::Number) if v.is_empty() => Value::Null,
                Some(FieldType::Number) => v.parse::<f64>().map(values::number).unwrap_or(json!(v)),
                Some(FieldType::Bool) => {
                    let l = v.to_ascii_lowercase();
                    json!(l == "true" || l == "1" || l == "yes")
                }
                Some(FieldType::Json | FieldType::File) if v.is_empty() => Value::Null,
                Some(FieldType::Json | FieldType::File)
                    if v.starts_with('{') || v.starts_with('[') =>
                {
                    serde_json::from_str(v).unwrap_or(json!(v))
                }
                Some(FieldType::Relation) if v.starts_with('[') => {
                    serde_json::from_str::<Vec<String>>(v)
                        .map(|ids| json!(ids))
                        .unwrap_or(json!(v))
                }
                _ if ["attempt", "max_attempts", "priority"].contains(&header.as_str()) => {
                    v.parse::<i64>().map(|n| json!(n)).unwrap_or(json!(v))
                }
                _ => json!(v),
            };
            rec.insert(header.clone(), value);
        }
        if has_values {
            out.push(rec);
        }
    }
    Ok(out)
}

/// Parses a JSON import: an array of records, an export envelope with
/// `records`, or one record.
pub fn parse_json(text: &str) -> Result<Vec<Map<String, Value>>, String> {
    if text.trim().is_empty() {
        return Err("empty json input".into());
    }
    let raw: Value =
        serde_json::from_str(text).map_err(|e| format!("invalid json payload: {e}"))?;
    match raw {
        Value::Array(items) => items
            .into_iter()
            .enumerate()
            .map(|(i, v)| match v {
                Value::Object(m) => Ok(m),
                _ => Err(format!("item at index {i} is not a JSON object")),
            })
            .collect(),
        Value::Object(mut m) => match m.remove("records") {
            Some(Value::Array(items)) => items
                .into_iter()
                .enumerate()
                .map(|(i, v)| match v {
                    Value::Object(m) => Ok(m),
                    _ => Err(format!("envelope records[{i}] is not a JSON object")),
                })
                .collect(),
            Some(other) => {
                m.insert("records".into(), other);
                Ok(vec![m])
            }
            None => Ok(vec![m]),
        },
        other => Err(format!(
            "expected JSON array or object, received {}",
            type_name(&other)
        )),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "<nil>",
        Value::Bool(_) => "bool",
        Value::Number(_) => "float64",
        Value::String(_) => "string",
        _ => "value",
    }
}

/// `GET /api/moul/:name/export?format=json|csv&includeSchema=&filter=&sort=`
pub(crate) fn export(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    cx.require_admin()?;
    let c = collection(cx, name)?;
    let format = cx.req.query_param("format").trim().to_ascii_lowercase();
    let format = if format.is_empty() {
        "json".to_owned()
    } else {
        format
    };
    if format != "csv" && format != "json" {
        return Err(ApiError::bad_request(
            "Invalid export format (supported: 'csv', 'json')",
        ));
    }
    let mut sql = format!("SELECT * FROM {}", quote(&c.name));
    let mut params: records::Params = Vec::new();
    let filter = cx.req.query_param("filter").to_owned();
    if !filter.trim().is_empty() {
        let (w, p) = rules::filter(cx, &c, &filter, None, None, "", false)
            .map_err(|e| ApiError::bad_request(format!("invalid export filter: {e}")))?
            .unwrap_or_default();
        if !w.is_empty() {
            sql += &format!(" WHERE {w}");
            params = p
                .into_iter()
                .map(|(k, v)| (k, values::json_to_sql(&v)))
                .collect();
        }
    }
    let sort_param = cx.req.query_param("sort").to_owned();
    let mut sort = rules::sort(cx, &c, &sort_param, false)
        .map_err(|e| ApiError::bad_request(format!("invalid export sort: {e}")))?;
    if sort.is_empty() {
        sort = vec!["createdAt DESC".into()];
    }
    let terms: Vec<String> = sort.iter().map(|t| records::quoted_term(t)).collect();
    sql += &format!(" ORDER BY {}", terms.join(", "));
    let mut rows = cx.guard.run(cx.conn, || {
        records::query(cx.conn, &sql, &params, Some(cx.meter))
    })??;
    for r in &mut rows {
        values::normalize(&c, r);
    }
    // Go's "2006-01-02-150405".
    let iso = time::rfc3339(cx.now_ms);
    let stamp = format!("{}-{}", &iso[..10], iso[11..19].replace(':', ""));
    let filename = format!("{}-records-{stamp}.{format}", c.name);
    let (content_type, data) = if format == "csv" {
        (
            "text/csv; charset=utf-8",
            to_csv(&rows, &column_names(&c)).into_bytes(),
        )
    } else {
        let body = if cx.req.query_param("includeSchema") == "true" {
            json!({
                "schema": c.to_json(),
                "exported_at": cx.now(),
                "total": rows.len(),
                "records": rows,
            })
        } else {
            json!(rows)
        };
        let mut s = serde_json::to_string_pretty(&body).expect("json serialises");
        s.push('\n');
        ("application/json; charset=utf-8", s.into_bytes())
    };
    Ok(Response::bytes(200, content_type, data).with_header(
        "Content-Disposition",
        &format!("attachment; filename={filename:?}"),
    ))
}

/// The columns and values one imported record writes. Port of Go's
/// `PrepareRecordForInsert`; credentials come back separately.
#[allow(clippy::type_complexity)]
fn prepare(
    cx: &mut Cx<'_>,
    c: &Collection,
    raw: &Map<String, Value>,
    now: &str,
) -> Result<(Vec<(String, Sql)>, Vec<(&'static str, String)>), String> {
    let mut out: Vec<(String, Sql)> = Vec::new();
    let mut secrets = Vec::new();
    let s = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let id = if id.is_empty() {
        ids::record_id(cx.host, &c.name)
    } else {
        id
    };
    out.push(("id".into(), Sql::Text(id)));
    for key in ["createdAt", "updatedAt"] {
        let v = raw
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(now);
        out.push((key.into(), Sql::Text(v.into())));
    }
    match c.kind {
        CollectionKind::Auth => {
            for key in ["username", "email"] {
                if let Some(v) = raw.get(key).and_then(Value::as_str) {
                    out.push((key.into(), Sql::Text(v.trim().into())));
                }
            }
            let hash = raw
                .get("passwordHash")
                .or_else(|| raw.get("password_hash"))
                .and_then(Value::as_str);
            if let Some(h) = hash {
                secrets.push(("passwordHash", h.to_owned()));
            }
        }
        CollectionKind::Worker => {
            for key in [
                "worker",
                "state",
                "queue",
                "args",
                "meta",
                "tags",
                "errors",
                "scheduled_at",
                "inserted_at",
                "attempted_at",
                "attempted_by",
                "cancelled_at",
                "completed_at",
                "discarded_at",
            ] {
                if let Some(v) = raw.get(key).filter(|v| !v.is_null()) {
                    out.push((key.into(), Sql::Text(s(v))));
                }
            }
            let has = |k: &str, out: &Vec<(String, Sql)>| out.iter().any(|(n, _)| n == k);
            if !has("args", &out) {
                let args = raw
                    .get("payload")
                    .filter(|v| !v.is_null())
                    .map(s)
                    .unwrap_or("{}".into());
                out.push(("args".into(), Sql::Text(args)));
            }
            for key in ["inserted_at", "scheduled_at"] {
                if !has(key, &out) {
                    out.push((key.into(), Sql::Text(now.into())));
                }
            }
            for key in ["priority", "attempt", "max_attempts"] {
                if let Some(v) = raw.get(key).filter(|v| !v.is_null()) {
                    let n = match v {
                        Value::Number(n) => n.as_f64().unwrap_or(0.0),
                        Value::String(t) => t.trim().parse().unwrap_or(0.0),
                        _ => 0.0,
                    };
                    out.push((key.into(), Sql::Integer(n as i64)));
                }
            }
        }
        CollectionKind::Analytic => {
            return Err("analytic collections are not supported by this engine yet".into());
        }
        CollectionKind::Base => {}
    }
    for f in &c.fields {
        let Some(val) = raw.get(&f.name).filter(|v| !v.is_null()) else {
            if f.required {
                return Err(format!("field {:?} is required", f.name));
            }
            continue;
        };
        let stored = match f.kind {
            FieldType::Text | FieldType::Email | FieldType::Url => {
                let t = s(val);
                if f.required && t.trim().is_empty() {
                    return Err(format!(
                        "field {:?} is required and cannot be empty",
                        f.name
                    ));
                }
                Sql::Text(t)
            }
            FieldType::Number => {
                let n = match val {
                    Value::Number(n) => n.as_f64(),
                    Value::String(t) => t.trim().parse().ok(),
                    _ => None,
                }
                .ok_or_else(|| format!("field {:?} must be a valid number", f.name))?;
                if let Some(min) = f.min.filter(|m| n < *m) {
                    return Err(format!(
                        "field {:?} must be >= {}",
                        f.name,
                        values::go_float(min)
                    ));
                }
                if let Some(max) = f.max.filter(|m| n > *m) {
                    return Err(format!(
                        "field {:?} must be <= {}",
                        f.name,
                        values::go_float(max)
                    ));
                }
                Sql::Real(n)
            }
            FieldType::Bool => Sql::Integer(match val {
                Value::Bool(b) => *b as i64,
                Value::String(t) => {
                    let l = t.trim().to_ascii_lowercase();
                    (l == "true" || l == "1" || l == "yes") as i64
                }
                Value::Number(n) => (n.as_f64() != Some(0.0)) as i64,
                _ => 0,
            }),
            FieldType::Date => {
                let Value::String(t) = val else {
                    return Err(format!(
                        "field {:?} must be a date string (YYYY-MM-DD)",
                        f.name
                    ));
                };
                let t = t.trim();
                if !t.is_empty() && time::parse_date(t).is_none() {
                    return Err(format!(
                        "field {:?} must be a valid date in YYYY-MM-DD format",
                        f.name
                    ));
                }
                Sql::Text(t.into())
            }
            FieldType::Datetime => {
                let Value::String(t) = val else {
                    return Err(format!(
                        "field {:?} must be an ISO 8601 date-time string",
                        f.name
                    ));
                };
                let t = t.trim();
                if !t.is_empty() && time::parse_datetime(t).is_none() {
                    return Err(format!(
                        "field {:?} must be a valid ISO 8601 date-time string",
                        f.name
                    ));
                }
                Sql::Text(t.into())
            }
            FieldType::Select => {
                let t = s(val);
                if f.required && t.trim().is_empty() {
                    return Err(format!("field {:?} is required", f.name));
                }
                if !t.is_empty() && !f.options.is_empty() && !f.options.contains(&t) {
                    return Err(format!(
                        "invalid value {t:?} for select field {:?} (allowed: {})",
                        f.name,
                        f.options.join(", ")
                    ));
                }
                Sql::Text(t)
            }
            FieldType::Json => match val {
                Value::String(t) => {
                    if !t.is_empty() && serde_json::from_str::<Value>(t).is_err() {
                        return Err(format!("field {:?} must be valid JSON", f.name));
                    }
                    Sql::Text(t.clone())
                }
                other => Sql::Text(other.to_string()),
            },
            FieldType::File => match val {
                Value::String(t) if !t.is_empty() && serde_json::from_str::<Value>(t).is_err() => {
                    Sql::Text(json!({"name": t}).to_string())
                }
                Value::String(t) => Sql::Text(t.clone()),
                other => Sql::Text(other.to_string()),
            },
            FieldType::Relation => {
                if f.relation.as_ref().is_some_and(|r| r.is_many()) {
                    match val {
                        Value::Array(items) => {
                            let ids: Vec<String> = items.iter().map(s).collect();
                            Sql::Text(serde_json::to_string(&ids).expect("ids serialise"))
                        }
                        Value::String(t) => Sql::Text(t.clone()),
                        _ => Sql::Text("[]".into()),
                    }
                } else {
                    Sql::Text(s(val))
                }
            }
            FieldType::Cloak => {
                return Err(format!("cloak field {:?} cannot be imported yet", f.name));
            }
            FieldType::Unknown(_) => values::json_to_sql(val),
        };
        out.push((f.name.clone(), stored));
    }
    Ok((out, secrets))
}

/// `POST /api/moul/:name/import?mode=upsert|insert|replace&onError=atomic|continue`
pub(crate) fn import(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    cx.require_admin()?;
    let c = collection(cx, name)?;
    let form_value = |key: &str| -> String {
        if let Body::Form { fields, .. } = &cx.req.body
            && let Some((_, v)) = fields.iter().find(|(k, _)| k == key)
        {
            return v.clone();
        }
        String::new()
    };
    let pick = |keys: &[&str]| -> String {
        keys.iter()
            .map(|k| {
                let f = form_value(k);
                if f.is_empty() {
                    cx.req.query_param(k).to_owned()
                } else {
                    f
                }
            })
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    let mode = pick(&["mode"]);
    let mode = if mode.is_empty() {
        "upsert".to_owned()
    } else {
        mode.trim().to_ascii_lowercase()
    };
    let on_error = pick(&["onError", "error_strategy"]);
    let on_error = if on_error.is_empty() {
        "atomic".to_owned()
    } else {
        on_error.trim().to_ascii_lowercase()
    };
    let mut format = pick(&["format"]).to_ascii_lowercase();

    let content: Vec<u8> = match &cx.req.body {
        Body::Form { files, .. } if files.iter().any(|(k, _)| k == "file") => {
            let (_, f) = files.iter().find(|(k, _)| k == "file").expect("checked");
            if format.is_empty() {
                let lower = f.name.to_ascii_lowercase();
                if lower.ends_with(".csv") {
                    format = "csv".into();
                } else if lower.ends_with(".json") {
                    format = "json".into();
                }
            }
            f.data.clone()
        }
        Body::Bytes { content_type, data } => {
            if format.is_empty() {
                if content_type.contains("text/csv") {
                    format = "csv".into();
                } else if content_type.contains("application/json") {
                    format = "json".into();
                }
            }
            data.clone()
        }
        Body::Json(v) => {
            if format.is_empty() {
                format = "json".into();
            }
            serde_json::to_vec(v).expect("json serialises")
        }
        // Bytes that did not parse as their declared type go through the
        // same checks, empty-body answer and format choice as Go's importer.
        Body::Invalid { content_type, data } => {
            if format.is_empty() && content_type.contains("application/json") {
                format = "json".into();
            }
            data.clone()
        }
        _ => Vec::new(),
    };

    let fail = |msg: String| ApiError::bad_request(format!("Import error: {msg}"));
    if !["upsert", "insert", "replace"].contains(&mode.as_str()) {
        return Err(fail(format!(
            "invalid import mode {mode:?}: must be 'upsert', 'insert', or 'replace'"
        )));
    }
    if !["atomic", "continue"].contains(&on_error.as_str()) {
        return Err(fail(format!(
            "invalid on-error behavior {on_error:?}: must be 'atomic' or 'continue'"
        )));
    }
    let text = String::from_utf8_lossy(&content).into_owned();
    let trimmed = text.trim();
    let empty_result = || json!({"success": true, "mode": mode, "total": 0, "inserted": 0, "updated": 0, "skipped": 0});
    if trimmed.is_empty() {
        return Ok(Response::json(200, empty_result()));
    }
    if format.is_empty() {
        format = if trimmed.starts_with('[') || trimmed.starts_with('{') {
            "json"
        } else {
            "csv"
        }
        .into();
    }
    let items = match format.as_str() {
        "json" => parse_json(trimmed).map_err(|e| fail(format!("json parsing error: {e}")))?,
        "csv" => parse_csv(trimmed, &c).map_err(|e| fail(format!("csv parsing error: {e}")))?,
        other => {
            return Err(fail(format!(
                "unsupported import format {other:?}: only 'csv' and 'json' are supported"
            )));
        }
    };

    let atomic = on_error == "atomic";
    cx.conn.execute_batch("SAVEPOINT import")?;
    // Who each record was before a replace: credentials stay only with a
    // record that comes back with the same id, username and email.
    let mut before: std::collections::HashMap<String, (Value, Value)> = Default::default();
    if mode == "replace" {
        if c.kind == CollectionKind::Auth {
            for r in records::query(
                cx.conn,
                &format!("SELECT id, username, email FROM {}", quote(&c.name)),
                &[],
                None,
            )? {
                let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
                let who = (
                    r.get("username").cloned().unwrap_or(Value::Null),
                    r.get("email").cloned().unwrap_or(Value::Null),
                );
                before.insert(id, who);
            }
        }
        cx.conn
            .execute(&format!("DELETE FROM {}", quote(&c.name)), [])?;
        cx.conn
            .execute("DELETE FROM main._file_refs WHERE moul = ?1", [&c.name])?;
    }
    let now = cx.now();
    let (mut inserted, mut updated, mut skipped) = (0, 0, 0);
    let mut errors: Vec<Value> = Vec::new();
    let mut failure: Option<String> = None;
    for (i, item) in items.iter().enumerate() {
        let row = i + 1;
        let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
        let row_error = |msg: String, id: Option<String>, errors: &mut Vec<Value>| {
            let mut e = Map::new();
            e.insert("row".into(), json!(row));
            if let Some(id) = id.filter(|i| !i.is_empty()) {
                e.insert("id".into(), json!(id));
            }
            e.insert("message".into(), json!(msg));
            errors.push(Value::Object(e));
        };
        let (data, secrets) = match prepare(cx, &c, item, &now) {
            Ok(d) => d,
            Err(msg) => {
                row_error(msg.clone(), item_id, &mut errors);
                if atomic {
                    failure = Some(format!("row {row} validation failed: {msg}"));
                    break;
                }
                skipped += 1;
                continue;
            }
        };
        let id = match &data[0].1 {
            Sql::Text(t) => t.clone(),
            _ => unreachable!("id is text"),
        };
        let exists = records::load(cx.conn, &c, &id)?.is_some();
        let result = if exists {
            if mode == "insert" {
                let msg = format!("record with ID {id:?} already exists");
                row_error(msg.clone(), Some(id.clone()), &mut errors);
                if atomic {
                    failure = Some(format!("row {row} error: {msg}"));
                    break;
                }
                skipped += 1;
                continue;
            }
            let sets: Vec<(String, Sql)> = data
                .into_iter()
                .filter(|(k, _)| k != "createdAt" && k != "id")
                .map(|(k, v)| {
                    if k == "updatedAt" {
                        (k, Sql::Text(now.clone()))
                    } else {
                        (k, v)
                    }
                })
                .collect();
            let clause: Vec<String> = sets
                .iter()
                .enumerate()
                .map(|(i, (k, _))| format!("{} = ?{}", quote(k), i + 1))
                .collect();
            let mut params: Vec<Sql> = sets.into_iter().map(|(_, v)| v).collect();
            params.push(Sql::Text(id.clone()));
            cx.conn
                .execute(
                    &format!(
                        "UPDATE {} SET {} WHERE id = ?{}",
                        quote(&c.name),
                        clause.join(", "),
                        params.len()
                    ),
                    rusqlite::params_from_iter(params),
                )
                .map(|_| true)
        } else {
            let cols: Vec<String> = data.iter().map(|(k, _)| quote(k)).collect();
            let marks: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
            cx.conn
                .execute(
                    &format!(
                        "INSERT INTO {} ({}) VALUES ({})",
                        quote(&c.name),
                        cols.join(", "),
                        marks.join(", ")
                    ),
                    rusqlite::params_from_iter(data.into_iter().map(|(_, v)| v)),
                )
                .map(|_| false)
        };
        match result {
            Ok(was_update) => {
                if let Some(old) = before.get(&id) {
                    let now_who = records::load(cx.conn, &c, &id)?.map(|r| {
                        (
                            r.get("username").cloned().unwrap_or(Value::Null),
                            r.get("email").cloned().unwrap_or(Value::Null),
                        )
                    });
                    if now_who.as_ref() != Some(old) {
                        cx.conn.execute(
                            "DELETE FROM writer._auth WHERE collection = ?1 AND record = ?2",
                            [&c.id, &id],
                        )?;
                    }
                }
                for (col, v) in secrets {
                    auth::set_secret(cx.conn, &c.id, &id, col, Some(&v))?;
                }
                if was_update {
                    updated += 1
                } else {
                    inserted += 1
                }
            }
            Err(e) => {
                // Some failures (a full disk, a spent budget) make SQLite
                // roll the whole transaction back: stop, nothing applies.
                if cx.conn.is_autocommit() {
                    return Err(ApiError::from(e));
                }
                let msg = e.to_string();
                row_error(msg.clone(), Some(id), &mut errors);
                if atomic {
                    failure = Some(format!(
                        "row {row} {} failed: {msg}",
                        if exists { "update" } else { "insert" }
                    ));
                    break;
                }
                skipped += 1;
            }
        }
    }
    let mut result = Map::new();
    result.insert("mode".into(), json!(mode));
    result.insert("total".into(), json!(items.len()));
    result.insert("inserted".into(), json!(inserted));
    result.insert("updated".into(), json!(updated));
    result.insert("skipped".into(), json!(skipped));
    if !errors.is_empty() {
        result.insert("errors".into(), json!(errors));
    }
    if let Some(msg) = failure {
        cx.conn
            .execute_batch("ROLLBACK TO import; RELEASE import")?;
        result.insert("success".into(), json!(false));
        // Logged as a refused request (its status is 400).
        cx.changed(&c, None, Vec::new());
        return Ok(Response::json(
            400,
            json!({"success": false, "error": msg, "result": result}),
        ));
    }
    if mode == "replace" {
        // Records the import did not bring back take their credentials.
        cx.conn.execute(
            &format!(
                "DELETE FROM writer._auth WHERE collection = ?1 AND record NOT IN (SELECT id FROM main.{})",
                quote(&c.name)
            ),
            [&c.id],
        )?;
    }
    cx.conn.execute_batch("RELEASE import")?;
    // Rebuild the file references of the whole collection.
    if c.fields.iter().any(|f| f.kind == FieldType::File) {
        let rows = records::query(
            cx.conn,
            &format!("SELECT * FROM {}", quote(&c.name)),
            &[],
            None,
        )?;
        for mut r in rows {
            values::normalize(&c, &mut r);
            files::sync_refs(cx, &c, &r)?;
        }
    }
    let success = if atomic {
        true
    } else {
        errors.is_empty() || inserted > 0 || updated > 0
    };
    result.insert("success".into(), json!(success));
    if inserted + updated > 0 || mode == "replace" {
        cx.changed(&c, None, Vec::new());
    }
    Ok(Response::json(200, Value::Object(result)))
}
