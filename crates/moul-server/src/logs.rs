//! The request log, visits and system metrics: port of the Go engine's
//! `/api/requests`, `/api/visits` and `/api/system/metrics`.
//!
//! Each request is queued after its answer and written in batches to the
//! writer-only store's `_requests` (method, path without the query, status,
//! time taken), under the `request_log_*` settings. Visits are kept in
//! `_visits`; this server reads the ones a converted Go database holds but
//! does not count new ones yet. Metrics are sampled every two seconds and
//! the last minute is kept.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::Method;
use axum::response::{IntoResponse, Response};
use rusqlite::params;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::http::{AppState, ClientIp, error, require_admin};

/// Paths never logged: the logs themselves, docs and MCP, as in Go.
const NOT_LOGGED: [&str; 14] = [
    "/api/visits",
    "/api/requests",
    "/api/workers",
    "/openapi.yml",
    "/openapi.json",
    "/docs",
    "/api/mcp",
    "/mcp",
    "/.well-known",
    "/api/oauth2",
    "/oauth2",
    "/AGENTS.md",
    "/llms.txt",
    "/llms-full.txt",
];

/// One logged request.
#[derive(Debug)]
struct Entry {
    method: String,
    path: String,
    status: u16,
    ms: u64,
    at: i64,
}

/// The request log's queue.
pub struct RequestLog {
    tx: mpsc::Sender<Entry>,
    rx: Mutex<Option<mpsc::Receiver<Entry>>>,
}

impl Default for RequestLog {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(1000);
        RequestLog {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }
}

impl RequestLog {
    /// Queues a request, if the settings say to log it.
    pub fn record(
        &self,
        state: &AppState,
        method: &Method,
        path: &str,
        status: u16,
        took: Duration,
    ) {
        if state.setting("request_log_enabled") == "false"
            || NOT_LOGGED.iter().any(|p| path.starts_with(p))
            || state
                .setting("request_log_exclude_paths")
                .split(',')
                .map(str::trim)
                .any(|p| !p.is_empty() && path.starts_with(p))
        {
            return;
        }
        let rate: f64 = state
            .setting("request_log_sample_rate")
            .parse()
            .unwrap_or(1.0);
        if rate < 1.0 {
            let mut b = [0u8; 4];
            getrandom::fill(&mut b).ok();
            if f64::from(u32::from_le_bytes(b)) / f64::from(u32::MAX) >= rate {
                return;
            }
        }
        let _ = self.tx.try_send(Entry {
            method: method.as_str().to_owned(),
            path: path.to_owned(),
            status,
            ms: took.as_millis() as u64,
            at: moul_vfs::system_clock(),
        });
    }

    /// Writes the queue to the store every five seconds or every hundred
    /// requests, until the server stops.
    pub fn spawn_writer(&self, state: AppState) {
        let Some(mut rx) = self.rx.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        tokio::spawn(async move {
            let mut batch = Vec::new();
            loop {
                let timeout = tokio::time::sleep(Duration::from_secs(5));
                tokio::pin!(timeout);
                let closed = loop {
                    tokio::select! {
                        got = rx.recv() => match got {
                            Some(e) => {
                                batch.push(e);
                                if batch.len() >= 100 { break false; }
                            }
                            None => break true,
                        },
                        () = &mut timeout => break false,
                    }
                };
                if !batch.is_empty() {
                    let rows = std::mem::take(&mut batch);
                    let _ = state.db.run(move |db| write_batch(db, rows)).await;
                }
                if closed {
                    return;
                }
            }
        });
    }
}

fn write_batch(db: &mut crate::db::Db, rows: Vec<Entry>) {
    let conn = db.engine.store().conn();
    if conn.execute_batch("BEGIN").is_err() {
        return;
    }
    let mut host = crate::open::OsRandom;
    for e in rows {
        let _ = conn.execute(
            "INSERT INTO writer._requests (id, visit_id, method, path, status_code, response_time_ms, createdAt)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6)",
            params![
                format!("req-{}", moul_core::ids::random_chars(&mut host, 15)),
                e.method,
                e.path,
                e.status,
                e.ms as i64,
                moul_core::time::rfc3339(e.at)
            ],
        );
    }
    let _ = conn.execute_batch("COMMIT");
}

/// Rows of a writer-only table as JSON objects.
fn rows(db: &mut crate::db::Db, sql: &str, binds: Vec<rusqlite::types::Value>) -> Vec<Value> {
    let conn = db.engine.store().conn();
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let names: Vec<String> = stmt.column_names().map(str::to_owned).collect();
    let mapped = stmt.query_map(rusqlite::params_from_iter(binds), |r| {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            let v = match r.get_ref(i)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(n) => json!(n),
                rusqlite::types::ValueRef::Real(f) => json!(f),
                rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Blob(_) => Value::Null,
            };
            m.insert(n.clone(), v);
        }
        Ok(Value::Object(m))
    });
    match mapped {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn count(db: &mut crate::db::Db, sql: &str, binds: Vec<rusqlite::types::Value>) -> i64 {
    db.engine
        .store()
        .conn()
        .query_row(sql, rusqlite::params_from_iter(binds), |r| r.get(0))
        .unwrap_or(0)
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Paging {
    page: Option<String>,
    per_page: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

fn ip_of(req: &Request) -> Option<std::net::IpAddr> {
    req.extensions().get::<ClientIp>().map(|c| c.0)
}

/// `GET /api/requests`: the log, newest first, in pages.
pub(crate) async fn requests(
    State(state): State<AppState>,
    Query(p): Query<Paging>,
    req: Request,
) -> Response {
    if let Err(r) = require_admin(&state, req.headers(), ip_of(&req)).await {
        return r;
    }
    let page = p
        .page
        .and_then(|v| v.parse().ok())
        .filter(|v: &i64| *v >= 1)
        .unwrap_or(1);
    let per_page = p
        .per_page
        .and_then(|v| v.parse().ok())
        .filter(|v: &i64| (1..=200).contains(v))
        .unwrap_or(50);
    let out = state
        .db
        .run(move |db| {
            let total = count(db, "SELECT count(*) FROM writer._requests", vec![]);
            let items = rows(
                db,
                &format!(
                    "SELECT * FROM writer._requests ORDER BY createdAt DESC, id LIMIT {per_page} OFFSET {}",
                    (page - 1) * per_page
                ),
                vec![],
            );
            json!({
                "page": page,
                "perPage": per_page,
                "totalItems": total,
                "totalPages": (total + per_page - 1) / per_page,
                "items": items,
            })
        })
        .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /api/requests/:id`
pub(crate) async fn request(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    if let Err(r) = require_admin(&state, req.headers(), ip_of(&req)).await {
        return r;
    }
    let found = state
        .db
        .run(move |db| {
            rows(
                db,
                "SELECT * FROM writer._requests WHERE id = ?1",
                vec![id.into()],
            )
        })
        .await;
    match found.map(|mut v| v.pop()) {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => error(404, "Request not found"),
        Err(e) => e.into_response(),
    }
}

/// `GET /api/visits`: without paging, the newest 200 as a list; with
/// `page` or `perPage`, in pages. `from` and `to` bound the start time.
pub(crate) async fn visits(
    State(state): State<AppState>,
    Query(p): Query<Paging>,
    req: Request,
) -> Response {
    if let Err(r) = require_admin(&state, req.headers(), ip_of(&req)).await {
        return r;
    }
    let paged = p.page.is_some() || p.per_page.is_some();
    let page = p
        .page
        .and_then(|v| v.parse().ok())
        .filter(|v: &i64| *v >= 1)
        .unwrap_or(1);
    let per_page = p
        .per_page
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v >= 1)
        .map_or(50, |v| v.min(200));
    let (from, to) = (p.from.unwrap_or_default(), p.to.unwrap_or_default());
    let out = state
        .db
        .run(move |db| {
            let mut wheres = Vec::new();
            let mut binds: Vec<rusqlite::types::Value> = Vec::new();
            if !from.is_empty() {
                wheres.push(format!("started_at >= ?{}", binds.len() + 1));
                binds.push(from.into());
            }
            if !to.is_empty() {
                wheres.push(format!("started_at <= ?{}", binds.len() + 1));
                binds.push(to.into());
            }
            let filter = if wheres.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", wheres.join(" AND "))
            };
            if !paged {
                return Value::Array(rows(
                    db,
                    &format!("SELECT * FROM writer._visits {filter} ORDER BY started_at DESC LIMIT 200"),
                    binds,
                ));
            }
            let total = count(db, &format!("SELECT count(*) FROM writer._visits {filter}"), binds.clone());
            let items = rows(
                db,
                &format!(
                    "SELECT * FROM writer._visits {filter} ORDER BY started_at DESC LIMIT {per_page} OFFSET {}",
                    (page - 1) * per_page
                ),
                binds,
            );
            json!({
                "page": page,
                "perPage": per_page,
                "totalItems": total,
                "totalPages": (total + per_page - 1) / per_page,
                "items": items,
            })
        })
        .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /api/visits/:id`
pub(crate) async fn visit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    if let Err(r) = require_admin(&state, req.headers(), ip_of(&req)).await {
        return r;
    }
    let found = state
        .db
        .run(move |db| {
            rows(
                db,
                "SELECT * FROM writer._visits WHERE id = ?1",
                vec![id.into()],
            )
        })
        .await;
    match found.map(|mut v| v.pop()) {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => error(404, "Visit not found"),
        Err(e) => e.into_response(),
    }
}

/// Sampled system metrics.
#[derive(Default)]
pub struct Metrics {
    history: Mutex<VecDeque<Value>>,
    pushed: Mutex<Map<String, Value>>,
}

/// How many samples are kept: a minute's worth.
const HISTORY: usize = 30;

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// What the operating system tells about the machine. Linux gives load,
/// memory and threads through `/proc`; elsewhere those stay zero.
fn sample(started: std::time::Instant) -> Value {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let load: Vec<f64> = read("/proc/loadavg")
        .split_whitespace()
        .take(3)
        .filter_map(|v| v.parse().ok())
        .collect();
    let meminfo = read("/proc/meminfo");
    let kb = |key: &str| -> u64 {
        meminfo
            .lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let (total, available, free) = (kb("MemTotal:"), kb("MemAvailable:"), kb("MemFree:"));
    let used = total.saturating_sub(available);
    let threads = read("/proc/self/status")
        .lines()
        .find(|l| l.starts_with("Threads:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    json!({
        "timestamp": moul_core::time::rfc3339(moul_vfs::system_clock()),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu": {"usage_active": 0.0, "usage_user": 0.0, "usage_system": 0.0, "usage_idle": 0.0, "cores": cores},
        "memory": {
            "used_percent": if total > 0 { used as f64 * 100.0 / total as f64 } else { 0.0 },
            "total": total, "used": used, "free": free, "available": available,
        },
        "disk": {"used_percent": 0.0, "total": 0, "used": 0, "free": 0, "path": "/"},
        "network": {"bytes_sent": 0, "bytes_recv": 0, "packets_sent": 0, "packets_recv": 0},
        "system": {
            "load1": load.first().copied().unwrap_or(0.0),
            "load5": load.get(1).copied().unwrap_or(0.0),
            "load15": load.get(2).copied().unwrap_or(0.0),
            "uptime": started.elapsed().as_secs(),
            "num_goroutines": threads,
        },
    })
}

impl Metrics {
    /// The current sample and the last minute's.
    pub fn snapshot(&self) -> Value {
        let h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let current = h.back().cloned().unwrap_or(Value::Null);
        json!({"current": current, "history": h.iter().cloned().collect::<Vec<_>>()})
    }

    /// Samples every two seconds until the server stops.
    pub fn spawn_sampler(state: AppState) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                let mut s = sample(state.started);
                // Values pushed by an agent such as Telegraf win.
                let pushed = state
                    .metrics
                    .pushed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                for (k, v) in pushed {
                    if let (Some(obj), Some(p)) = (s[&k].as_object_mut(), v.as_object()) {
                        obj.extend(p.clone());
                    }
                }
                let mut h = state
                    .metrics
                    .history
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                h.push_back(s);
                while h.len() > HISTORY {
                    h.pop_front();
                }
            }
        });
    }
}

/// `GET /api/system/metrics`
pub(crate) async fn metrics_get(State(state): State<AppState>, req: Request) -> Response {
    if let Err(r) = require_admin(&state, req.headers(), ip_of(&req)).await {
        return r;
    }
    let h = state
        .metrics
        .history
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let current = h.back().cloned().unwrap_or_else(|| sample(state.started));
    Json(json!({"current": current, "history": h.iter().cloned().collect::<Vec<_>>()}))
        .into_response()
}

/// `POST /api/system/metrics`: a Telegraf-style point, a list of them, or
/// `{"metrics": […]}`. `cpu`, `mem`, `disk`, `net` and `system` points are
/// kept and shown until the next push.
pub(crate) async fn metrics_post(State(state): State<AppState>, req: Request) -> Response {
    if let Err(r) = require_admin(&state, req.headers(), ip_of(&req)).await {
        return r;
    }
    let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let points: Vec<Value> = match v {
        Value::Array(a) => a,
        Value::Object(ref o) if o.get("metrics").is_some_and(Value::is_array) => {
            o["metrics"].as_array().cloned().unwrap_or_default()
        }
        Value::Object(_) => vec![v],
        _ => Vec::new(),
    };
    let mut pushed = state
        .metrics
        .pushed
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for p in points {
        let fields = p["fields"].as_object().cloned().unwrap_or_default();
        let section = match p["name"].as_str().unwrap_or("") {
            "cpu" => "cpu",
            "mem" => "memory",
            "disk" => "disk",
            "net" => "network",
            "system" => "system",
            _ => continue,
        };
        let entry = pushed.entry(section).or_insert_with(|| json!({}));
        if let Some(o) = entry.as_object_mut() {
            o.extend(fields);
        }
    }
    Json(json!({"message": "metrics ingested successfully", "status": "ok"})).into_response()
}
