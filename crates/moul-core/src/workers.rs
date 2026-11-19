//! Background jobs: port of the Go engine's `_workers` table and its
//! `/api/workers` endpoints, kept in the writer-only store because a job's
//! arguments can hold mail with sign-in codes.
//!
//! A job names a worker. The native server runs the workers it has built in
//! (`SendEmail`, `Webhook`); a job for any other worker waits, `available`,
//! for a program of the builder's that claims it through the API. A deployed
//! app carries no code of its own, so nothing else can run one.
//!
//! | Path | Methods | Who |
//! | --- | --- | --- |
//! | `/api/workers` | `GET`, `POST` | root users and the owner |
//! | `/api/workers/:id` | `GET`, `PATCH`, `DELETE` | root users and the owner |
//! | `/api/workers/retry` | `POST` | root users and the owner |
//!
//! Where this departs from Go: list and read answers carry numbers and JSON
//! as numbers and JSON (Go sent every column as a string there, and decoded
//! them only when creating); `state` and `scheduled_at` are checked; a job
//! left `executing` by a crash goes back to `available` after a lease; and
//! discarded jobs are kept seven days, like completed ones, so they can be
//! retried.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Response};
use crate::engine::Cx;
use crate::host::Host;
use crate::{ids, records, time};

/// The states a job can be in.
pub const STATES: [&str; 6] = [
    "available",
    "executing",
    "completed",
    "discarded",
    "cancelled",
    "retryable",
];

const JSON_COLUMNS: [&str; 4] = ["args", "meta", "tags", "errors"];
const INT_COLUMNS: [&str; 3] = ["attempt", "max_attempts", "priority"];
const SORTABLE: [&str; 14] = [
    "id",
    "createdAt",
    "updatedAt",
    "state",
    "queue",
    "worker",
    "attempt",
    "max_attempts",
    "priority",
    "inserted_at",
    "scheduled_at",
    "attempted_at",
    "completed_at",
    "discarded_at",
];

/// A job row as JSON: JSON columns decoded, counts as numbers.
fn shape(mut row: Map<String, Value>) -> Value {
    for c in JSON_COLUMNS {
        if let Some(Value::String(s)) = row.get(c) {
            let v = serde_json::from_str(s).unwrap_or(Value::String(s.clone()));
            row.insert(c.into(), v);
        }
    }
    for c in INT_COLUMNS {
        if let Some(Value::String(s)) = row.get(c)
            && let Ok(n) = s.parse::<i64>()
        {
            row.insert(c.into(), json!(n));
        }
    }
    Value::Object(row)
}

fn load(conn: &Connection, id: &str) -> ApiResult<Option<Value>> {
    Ok(records::query(
        conn,
        "SELECT * FROM writer._workers WHERE id = :v",
        &[("v".into(), rusqlite::types::Value::Text(id.into()))],
        None,
    )?
    .into_iter()
    .next()
    .map(shape))
}

fn body(cx: &Cx<'_>) -> ApiResult<Map<String, Value>> {
    match cx.req.json_object() {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Ok(Map::new()),
        Err(()) => Err(ApiError::bad_request("Invalid request body")),
    }
}

/// `GET /api/workers`
pub(crate) fn list(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let page = cx
        .req
        .query_param("page")
        .parse::<i64>()
        .ok()
        .filter(|p| *p >= 1)
        .unwrap_or(1);
    let per_page = cx
        .req
        .query_param("perPage")
        .parse::<i64>()
        .ok()
        .filter(|p| (1..=200).contains(p))
        .unwrap_or(50);
    let mut wheres = Vec::new();
    let mut binds: Vec<(String, rusqlite::types::Value)> = Vec::new();
    for key in ["state", "queue", "worker"] {
        let v = cx.req.query_param(key);
        if !v.is_empty() {
            wheres.push(format!("{key} = :{key}"));
            binds.push((key.into(), rusqlite::types::Value::Text(v.into())));
        }
    }
    let filter = if wheres.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", wheres.join(" AND "))
    };
    let sort = cx.req.query_param("sort");
    let order = {
        let (desc, col) = match sort.strip_prefix('-') {
            Some(c) => (true, c),
            None => (false, sort.strip_prefix('+').unwrap_or(sort)),
        };
        if SORTABLE.contains(&col) {
            format!("{col} {}, id", if desc { "DESC" } else { "ASC" })
        } else {
            "createdAt DESC, id".into()
        }
    };
    let total: i64 = {
        let rows = records::query(
            cx.conn,
            &format!("SELECT count(*) AS n FROM writer._workers {filter}"),
            &binds,
            None,
        )?;
        rows.first()
            .and_then(|r| r.get("n"))
            .and_then(|n| {
                n.as_i64()
                    .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0)
    };
    let items: Vec<Value> = records::query(
        cx.conn,
        &format!(
            "SELECT * FROM writer._workers {filter} ORDER BY {order} LIMIT {per_page} OFFSET {}",
            (page - 1) * per_page
        ),
        &binds,
        Some(cx.meter),
    )?
    .into_iter()
    .map(shape)
    .collect();
    Ok(Response::json(
        200,
        json!({
            "page": page,
            "perPage": per_page,
            "totalItems": total,
            "totalPages": (total + per_page - 1) / per_page,
            "items": items,
        }),
    ))
}

/// `GET /api/workers/:id`
pub(crate) fn get(cx: &mut Cx<'_>, id: &str) -> ApiResult {
    cx.require_admin()?;
    load(cx.conn, id)?
        .map(|j| Response::json(200, j))
        .ok_or_else(|| ApiError::not_found("Worker job not found"))
}

/// A time a client gave, as the stored form: RFC 3339 in UTC, so that
/// times compare as strings.
fn utc(s: &str) -> Option<String> {
    time::parse_rfc3339(s).map(time::rfc3339)
}

/// Adds a job. `fields` is what `POST /api/workers` takes.
pub(crate) fn enqueue(
    conn: &Connection,
    host: &mut dyn Host,
    now_ms: i64,
    fields: &Map<String, Value>,
) -> ApiResult<Value> {
    let worker = fields
        .get("worker")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if worker.is_empty() {
        return Err(ApiError::bad_request("worker field is required"));
    }
    let id = match fields.get("id").and_then(Value::as_str).map(str::trim) {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => format!("_worker-{}", ids::random_chars(host, 15)),
    };
    let now = time::rfc3339(now_ms);
    let scheduled = match fields
        .get("scheduled_at")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(s) => utc(s).ok_or_else(|| {
            ApiError::bad_request(
                "scheduled_at must be an RFC 3339 time, such as 2026-11-19T09:00:00Z",
            )
        })?,
        None => now.clone(),
    };
    let text = |k: &str, default: &str| {
        fields
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_owned()
    };
    let json_of = |k: &str, default: Value| -> String {
        match fields.get(k) {
            None | Some(Value::Null) => default.to_string(),
            Some(v) => v.to_string(),
        }
    };
    let int = |k: &str, default: i64| fields.get(k).and_then(Value::as_i64).unwrap_or(default);
    let max_attempts = int("max_attempts", 20);
    if max_attempts < 1 {
        return Err(ApiError::bad_request("max_attempts must be at least 1"));
    }
    let n = conn.execute(
        "INSERT OR IGNORE INTO writer._workers (id, createdAt, updatedAt, state, queue, worker, args, meta, tags,
           errors, attempt, max_attempts, priority, inserted_at, scheduled_at)
         VALUES (?1, ?2, ?2, 'available', ?3, ?4, ?5, ?6, ?7, '[]', 0, ?8, ?9, ?2, ?10)",
        params![
            id,
            now,
            text("queue", "default"),
            worker,
            json_of("args", json!({})),
            json_of("meta", json!({})),
            json_of("tags", json!([])),
            max_attempts,
            int("priority", 0),
            scheduled
        ],
    )?;
    if n == 0 {
        return Err(ApiError::bad_request("A job with this id already exists"));
    }
    load(conn, &id)?.ok_or_else(ApiError::internal)
}

/// `POST /api/workers`
pub(crate) fn create(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let b = body(cx)?;
    let job = enqueue(cx.conn, cx.host, cx.now_ms, &b)?;
    Ok(Response::json(201, job))
}

/// `PATCH /api/workers/:id`: `state`, `scheduled_at`, `priority` and
/// `max_attempts`. Making a job `available` starts its attempts again.
pub(crate) fn update(cx: &mut Cx<'_>, id: &str) -> ApiResult {
    cx.require_admin()?;
    let b = body(cx)?;
    if load(cx.conn, id)?.is_none() {
        return Err(ApiError::not_found("Worker job not found"));
    }
    let now = cx.now();
    let mut sets: Vec<(String, rusqlite::types::Value)> = Vec::new();
    let mut scheduled = None;
    if let Some(s) = b.get("scheduled_at").and_then(Value::as_str) {
        let t = utc(s).ok_or_else(|| {
            ApiError::bad_request(
                "scheduled_at must be an RFC 3339 time, such as 2026-11-19T09:00:00Z",
            )
        })?;
        scheduled = Some(t.clone());
        sets.push(("scheduled_at".into(), t.into()));
    }
    for k in ["priority", "max_attempts"] {
        if let Some(n) = b.get(k).and_then(Value::as_i64) {
            sets.push((k.into(), n.into()));
        }
    }
    if let Some(state) = b.get("state").and_then(Value::as_str) {
        if !STATES.contains(&state) {
            return Err(ApiError::bad_request(format!(
                "state must be one of {}",
                STATES.join(", ")
            )));
        }
        sets.push(("state".into(), state.to_owned().into()));
        match state {
            "available" => {
                sets.push(("attempt".into(), 0i64.into()));
                if scheduled.is_none() {
                    sets.push(("scheduled_at".into(), now.clone().into()));
                }
            }
            "discarded" => sets.push(("discarded_at".into(), now.clone().into())),
            "cancelled" => sets.push(("cancelled_at".into(), now.clone().into())),
            "completed" => sets.push(("completed_at".into(), now.clone().into())),
            _ => {}
        }
    }
    sets.push(("updatedAt".into(), now.into()));
    for (col, value) in sets {
        cx.conn.execute(
            &format!("UPDATE writer._workers SET {col} = ?1 WHERE id = ?2"),
            params![value, id],
        )?;
    }
    Ok(Response::json(
        200,
        load(cx.conn, id)?.ok_or_else(ApiError::internal)?,
    ))
}

/// `DELETE /api/workers/:id`
pub(crate) fn delete(cx: &mut Cx<'_>, id: &str) -> ApiResult {
    cx.require_admin()?;
    let n = cx
        .conn
        .execute("DELETE FROM writer._workers WHERE id = ?1", [id])?;
    if n == 0 {
        return Err(ApiError::not_found("Worker job not found"));
    }
    Ok(Response::json(200, json!({"success": true})))
}

/// `POST /api/workers/retry` with `{"ids": […]}`, or for every discarded,
/// cancelled or retryable job.
pub(crate) fn retry(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let b = body(cx)?;
    let now = cx.now();
    let n = match b.get("ids").and_then(Value::as_array) {
        Some(ids) => {
            let mut n = 0;
            for id in ids.iter().filter_map(Value::as_str) {
                n += cx.conn.execute(
                    "UPDATE writer._workers SET state = 'available', scheduled_at = ?1, attempt = 0, updatedAt = ?1
                     WHERE id = ?2",
                    params![now, id],
                )?;
            }
            n
        }
        None => cx.conn.execute(
            "UPDATE writer._workers SET state = 'available', scheduled_at = ?1, attempt = 0, updatedAt = ?1
             WHERE state IN ('discarded', 'cancelled', 'retryable')",
            [&now],
        )?,
    };
    Ok(Response::json(
        200,
        json!({"success": true, "rows_affected": n}),
    ))
}

/// A job claimed for running.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// Its id.
    pub id: String,
    /// The worker that runs it.
    pub worker: String,
    /// Its arguments.
    pub args: Value,
    /// This attempt's number, from 1.
    pub attempt: i64,
    /// How many attempts it gets.
    pub max_attempts: i64,
}

/// Claims up to `limit` due jobs for `workers`, lowest priority number
/// first, marking them `executing` by `node`.
pub(crate) fn claim(
    conn: &Connection,
    workers: &[&str],
    limit: usize,
    node: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<Job>> {
    if workers.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let now = time::rfc3339(now_ms);
    let marks = vec!["?"; workers.len()].join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM writer._workers WHERE state = 'available' AND scheduled_at <= ?
         AND worker IN ({marks}) ORDER BY priority ASC, scheduled_at ASC, id ASC LIMIT {limit}"
    ))?;
    let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&now];
    for w in workers {
        binds.push(w);
    }
    let ids: Vec<String> = stmt
        .query_map(binds.as_slice(), |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for id in ids {
        let job = conn
            .query_row(
                "UPDATE writer._workers SET state = 'executing', attempt = attempt + 1,
                   attempted_at = ?1, attempted_by = ?2, updatedAt = ?1
                 WHERE id = ?3 AND state = 'available'
                 RETURNING id, worker, args, attempt, max_attempts",
                params![now, node, id],
                |r| {
                    let args: Option<String> = r.get(2)?;
                    Ok(Job {
                        id: r.get(0)?,
                        worker: r.get(1)?,
                        args: args
                            .and_then(|a| serde_json::from_str(&a).ok())
                            .unwrap_or(Value::Null),
                        attempt: r.get::<_, Option<i64>>(3)?.unwrap_or(1),
                        max_attempts: r.get::<_, Option<i64>>(4)?.unwrap_or(20),
                    })
                },
            )
            .optional()?;
        out.extend(job);
    }
    Ok(out)
}

/// Records how a claimed job went: `completed`, or the error and either
/// another attempt (after `attempt² × 10 + 10` seconds plus up to 30, as Go
/// waits) or `discarded` once its attempts are used.
pub(crate) fn finish(
    conn: &Connection,
    host: &mut dyn Host,
    id: &str,
    outcome: Result<(), String>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let now = time::rfc3339(now_ms);
    match outcome {
        Ok(()) => {
            conn.execute(
                "UPDATE writer._workers SET state = 'completed', completed_at = ?1, updatedAt = ?1
                 WHERE id = ?2 AND state = 'executing'",
                params![now, id],
            )?;
        }
        Err(e) => {
            let row: Option<(i64, i64, Option<String>)> = conn
                .query_row(
                    "SELECT attempt, max_attempts, errors FROM writer._workers WHERE id = ?1",
                    [id],
                    |r| {
                        Ok((
                            r.get::<_, Option<i64>>(0)?.unwrap_or(1),
                            r.get::<_, Option<i64>>(1)?.unwrap_or(20),
                            r.get(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((attempt, max, errors)) = row else {
                return Ok(());
            };
            let mut list: Vec<Value> = errors
                .and_then(|e| serde_json::from_str(&e).ok())
                .unwrap_or_default();
            list.push(json!(format!("[{now}] {e}")));
            let errors = Value::Array(list).to_string();
            if attempt >= max {
                conn.execute(
                    "UPDATE writer._workers SET state = 'discarded', discarded_at = ?1, errors = ?2, updatedAt = ?1
                     WHERE id = ?3 AND state = 'executing'",
                    params![now, errors, id],
                )?;
            } else {
                let mut r = [0u8; 1];
                host.fill_random(&mut r);
                let wait_s = attempt * attempt * 10 + 10 + i64::from(r[0] % 30);
                conn.execute(
                    "UPDATE writer._workers SET state = 'available', scheduled_at = ?1, errors = ?2, updatedAt = ?3
                     WHERE id = ?4 AND state = 'executing'",
                    params![time::rfc3339(now_ms + wait_s * 1000), errors, now, id],
                )?;
            }
        }
    }
    Ok(())
}

/// Puts jobs back that have been `executing` for longer than `lease_ms`:
/// their runner stopped before it said how they went.
pub(crate) fn rescue(conn: &Connection, lease_ms: i64, now_ms: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE writer._workers SET state = 'available', scheduled_at = ?1, updatedAt = ?1
         WHERE state = 'executing' AND attempted_at < ?2",
        params![time::rfc3339(now_ms), time::rfc3339(now_ms - lease_ms)],
    )
}

/// Deletes completed and discarded jobs older than `keep_ms`.
pub(crate) fn prune(conn: &Connection, keep_ms: i64, now_ms: i64) -> rusqlite::Result<usize> {
    let cutoff = time::rfc3339(now_ms - keep_ms);
    conn.execute(
        "DELETE FROM writer._workers WHERE (state = 'completed' AND completed_at < ?1)
           OR (state = 'discarded' AND discarded_at < ?1)",
        [cutoff],
    )
}
