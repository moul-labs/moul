//! MCP: the database as tools for an AI agent, over Streamable HTTP at
//! `/api/mcp` (root users and the admin key) and over stdio (`moul mcp
//! <folder>`, a local file the agent already holds).
//!
//! The tools keep the Go engine's names and arguments, so an agent set up
//! for it works unchanged, and add a few of Moul's own. Where Go wrote
//! straight into tables, skipping rules, validation and realtime, these
//! send the engine the same requests Studio sends, as the owner: every
//! change is checked, logged in the change log under the owner, and
//! announced to live queries. Answers are the engine's JSON, not Go's rows
//! of strings.
//!
//! | Tool | Does |
//! | --- | --- |
//! | `moul_list_collections`, `moul_get_collection`, `moul_create_collection`, `moul_delete_collection` | collections |
//! | `moul_list_records`, `moul_get_record`, `moul_create_record`, `moul_update_record`, `moul_delete_record` | records |
//! | `moul_list_worker_jobs`, `moul_enqueue_job`, `moul_cancel_job` | jobs |
//! | `moul_list_feature_flags`, `moul_set_feature_flag` | feature flags |
//! | `moul_get_system_metrics`, `moul_get_analytics_summary`, `moul_list_requests` | the server |
//! | `moul_get_schema`, `moul_apply_schema` | the schema file (Moul) |
//! | `moul_list_changes` | the change log (Moul) |
//! | `moul_query` | one read-only `SELECT` over the shared store; secrets are out of reach (Moul) |

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::db::DbHandle;
use crate::http::{AppState, ClientIp, require_admin};
use crate::logs::Metrics;

/// Protocol versions this server speaks, newest first.
pub const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// What the tools reach.
pub struct Tools<'a> {
    /// The engine's thread.
    pub db: &'a DbHandle,
    /// The name in `serverInfo`.
    pub name: String,
    /// Metrics, on a server.
    pub metrics: Option<(&'a Metrics, std::time::Instant)>,
}

fn prop(kind: &str, description: &str) -> Value {
    json!({"type": kind, "description": description})
}

fn tool(name: &str, description: &str, props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut p = Map::new();
    for (k, v) in props {
        p.insert((*k).to_owned(), v.clone());
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": p, "required": required},
        "annotations": {"readOnlyHint": name.contains("_list_") || name.contains("_get_") || name == "moul_query"},
    })
}

/// Every tool, by name.
pub fn list() -> Vec<Value> {
    let s = |d| prop("string", d);
    let i = |d| prop("integer", d);
    let b = |d| prop("boolean", d);
    let mut tools = vec![
        tool(
            "moul_list_collections",
            "List all dynamic collections/tables defined in moul-dev",
            &[],
            &[],
        ),
        tool(
            "moul_get_collection",
            "Get detailed schema and rules for a dynamic collection by name",
            &[("name", s("Name of the collection"))],
            &["name"],
        ),
        tool(
            "moul_create_collection",
            "Create a new dynamic collection schema and SQLite table",
            &[
                ("name", s("Name of the collection")),
                (
                    "type",
                    s("Collection type: base, auth, worker, or analytic (default: base)"),
                ),
                (
                    "fields_json",
                    s(
                        "JSON array of field definitions (e.g. [{\"name\":\"title\",\"type\":\"text\"}])",
                    ),
                ),
            ],
            &["name"],
        ),
        tool(
            "moul_delete_collection",
            "Delete a dynamic collection and drop its SQLite table",
            &[("name", s("Name of the collection to delete"))],
            &["name"],
        ),
        tool(
            "moul_list_records",
            "List records from a specified collection",
            &[
                ("collection", s("Collection/table name")),
                ("page", i("Page number (default: 1)")),
                ("per_page", i("Records per page (default: 30)")),
                (
                    "filter",
                    s("A filter in the rule language, such as done = false (Moul)"),
                ),
                ("sort", s("Fields to sort by, such as -createdAt (Moul)")),
            ],
            &["collection"],
        ),
        tool(
            "moul_get_record",
            "Get a single record by ID from a collection",
            &[
                ("collection", s("Collection/table name")),
                ("id", s("Record ID")),
            ],
            &["collection", "id"],
        ),
        tool(
            "moul_create_record",
            "Create a new record in a collection",
            &[
                ("collection", s("Collection/table name")),
                ("data_json", s("JSON object representing the record data")),
            ],
            &["collection", "data_json"],
        ),
        tool(
            "moul_update_record",
            "Update an existing record in a collection",
            &[
                ("collection", s("Collection/table name")),
                ("id", s("Record ID")),
                ("data_json", s("JSON object with fields to update")),
            ],
            &["collection", "id", "data_json"],
        ),
        tool(
            "moul_delete_record",
            "Delete a record by ID from a collection",
            &[
                ("collection", s("Collection/table name")),
                ("id", s("Record ID")),
            ],
            &["collection", "id"],
        ),
        tool(
            "moul_list_worker_jobs",
            "List background jobs from a worker table",
            &[
                ("table", s("Worker table name (default: _workers)")),
                (
                    "state",
                    s(
                        "Filter by state: available, executing, completed, retryable, cancelled, discarded",
                    ),
                ),
                ("limit", i("Max records to fetch (default: 50)")),
            ],
            &[],
        ),
        tool(
            "moul_enqueue_job",
            "Enqueue a new background job into a worker table",
            &[
                ("table", s("Worker table name (default: _workers)")),
                ("worker", s("Worker handler name (e.g. SendEmail)")),
                ("args_json", s("JSON object with job arguments")),
                ("queue", s("Queue name (default: default)")),
                (
                    "priority",
                    i("Job priority (lower numbers run first, default: 0)"),
                ),
            ],
            &["worker"],
        ),
        tool(
            "moul_cancel_job",
            "Cancel a pending or retryable worker job",
            &[
                ("table", s("Worker table name (default: _workers)")),
                ("id", s("Job ID")),
            ],
            &["id"],
        ),
        tool(
            "moul_list_feature_flags",
            "List all feature flags stored in moul-dev",
            &[],
            &[],
        ),
        tool(
            "moul_set_feature_flag",
            "Create or update a feature flag",
            &[
                ("key", s("Unique key of the feature flag")),
                ("enabled", b("Enable or disable flag")),
                (
                    "description",
                    s("Description of what the feature flag controls"),
                ),
                ("default_value", s("Default string/boolean/number value")),
            ],
            &["key", "enabled"],
        ),
        tool(
            "moul_get_system_metrics",
            "Get current host system resource usage metrics (CPU, RAM, Disk, Load)",
            &[],
            &[],
        ),
        tool(
            "moul_get_analytics_summary",
            "Get analytics summary including total visitors and HTTP request statistics",
            &[],
            &[],
        ),
        tool(
            "moul_list_requests",
            "List recent HTTP request logs captured by moul-dev tracking middleware",
            &[("limit", i("Max records to return (default: 30)"))],
            &[],
        ),
        tool(
            "moul_get_schema",
            "Get the app definition as a schema file: collections, fields with their ids, and rules",
            &[],
            &[],
        ),
        tool(
            "moul_apply_schema",
            "Apply a schema file: create and change collections to match it",
            &[
                (
                    "schema_json",
                    s("The schema file, as moul_get_schema returns it"),
                ),
                ("prune", b("Also delete collections the file does not list")),
            ],
            &["schema_json"],
        ),
        tool(
            "moul_list_changes",
            "List the change log: one entry for each write, applied or refused, oldest first",
            &[
                ("after", i("Only entries after this position (default: 0)")),
                ("limit", i("Max entries (default: 50)")),
            ],
            &[],
        ),
        tool(
            "moul_query",
            "Run one read-only SQL SELECT over the app's data (the shared store); credentials and keys are out of reach. At most 500 rows.",
            &[("sql", s("A single SELECT statement"))],
            &["sql"],
        ),
    ];
    tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    tools
}

fn text_result(v: &Value) -> Value {
    let text = match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    json!({"content": [{"type": "text", "text": text}]})
}

fn error_result(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

fn arg_str(args: &Value, k: &str) -> String {
    match &args[k] {
        Value::String(s) => s.trim().to_owned(),
        _ => String::new(),
    }
}

fn arg_int(args: &Value, k: &str) -> Option<i64> {
    match &args[k] {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn arg_json(args: &Value, k: &str) -> Result<Option<Value>, String> {
    match &args[k] {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => serde_json::from_str(s)
            .map(Some)
            .map_err(|e| format!("{k} is not valid JSON: {e}")),
        other => Ok(Some(other.clone())),
    }
}

fn enc(s: &str) -> String {
    moul_core::oauth::encode(s)
}

impl Tools<'_> {
    /// Sends the engine a request as the owner; the JSON answer, or the
    /// error message.
    async fn call(&self, req: moul_core::Request) -> Result<Value, String> {
        let r = self.db.handle(req.owner()).await;
        if r.status >= 400 {
            let b = r.json_body();
            let m = b["message"]
                .as_str()
                .or(b["error"].as_str())
                .unwrap_or("request failed");
            return Err(format!("{m} ({})", r.status));
        }
        Ok(r.json_body().clone())
    }

    /// Runs a tool.
    pub async fn run(&self, name: &str, args: &Value) -> Value {
        match self.run_inner(name, args).await {
            Ok(v) => text_result(&v),
            Err(e) => error_result(&e),
        }
    }

    async fn run_inner(&self, name: &str, args: &Value) -> Result<Value, String> {
        use moul_core::Request as R;
        let need = |k: &str| -> Result<String, String> {
            let v = arg_str(args, k);
            if v.is_empty() {
                Err(format!("{k} is required"))
            } else {
                Ok(v)
            }
        };
        match name {
            "moul_list_collections" => self.call(R::get("/api/moul")).await,
            "moul_get_collection" => {
                self.call(R::get(&format!("/api/moul/{}", enc(&need("name")?))))
                    .await
            }
            "moul_create_collection" => {
                let mut body = json!({"name": need("name")?});
                let kind = arg_str(args, "type");
                body["type"] = json!(if ["base", "auth", "worker", "analytic"]
                    .contains(&kind.as_str())
                {
                    kind
                } else {
                    "base".into()
                });
                body["fields"] = arg_json(args, "fields_json")?.unwrap_or_else(|| json!([]));
                self.call(R::post("/api/moul", body)).await
            }
            "moul_delete_collection" => {
                let n = need("name")?;
                self.call(R::delete(&format!("/api/moul/{}", enc(&n))))
                    .await?;
                Ok(json!(format!("Collection \"{n}\" deleted successfully")))
            }
            "moul_list_records" => {
                let c = need("collection")?;
                let page = arg_int(args, "page").filter(|p| *p >= 1).unwrap_or(1);
                let per = arg_int(args, "per_page")
                    .filter(|p| (1..=500).contains(p))
                    .unwrap_or(30);
                let mut r = R::get(&format!("/api/moul/{}/records", enc(&c)))
                    .query("page", &page.to_string())
                    .query("perPage", &per.to_string());
                for k in ["filter", "sort"] {
                    let v = arg_str(args, k);
                    if !v.is_empty() {
                        r = r.query(k, &v);
                    }
                }
                self.call(r).await
            }
            "moul_get_record" => {
                self.call(R::get(&format!(
                    "/api/moul/{}/records/{}",
                    enc(&need("collection")?),
                    enc(&need("id")?)
                )))
                .await
            }
            "moul_create_record" => {
                let data = arg_json(args, "data_json")?.ok_or("data_json is required")?;
                self.call(R::post(
                    &format!("/api/moul/{}/records", enc(&need("collection")?)),
                    data,
                ))
                .await
            }
            "moul_update_record" => {
                let data = arg_json(args, "data_json")?.ok_or("data_json is required")?;
                self.call(R::patch(
                    &format!(
                        "/api/moul/{}/records/{}",
                        enc(&need("collection")?),
                        enc(&need("id")?)
                    ),
                    data,
                ))
                .await
            }
            "moul_delete_record" => {
                let (c, id) = (need("collection")?, need("id")?);
                self.call(R::delete(&format!(
                    "/api/moul/{}/records/{}",
                    enc(&c),
                    enc(&id)
                )))
                .await?;
                Ok(json!(format!(
                    "Record \"{id}\" deleted successfully from collection \"{c}\""
                )))
            }
            "moul_list_worker_jobs" => {
                let table = arg_str(args, "table");
                if !table.is_empty() && table != "_workers" {
                    return Err("only the _workers table holds jobs on this server".into());
                }
                let limit = arg_int(args, "limit")
                    .filter(|l| (1..=200).contains(l))
                    .unwrap_or(50);
                let mut r = R::get("/api/workers").query("perPage", &limit.to_string());
                let st = arg_str(args, "state");
                if !st.is_empty() {
                    r = r.query("state", &st);
                }
                Ok(self.call(r).await?["items"].clone())
            }
            "moul_enqueue_job" => {
                let mut body = json!({"worker": need("worker")?});
                if let Some(a) = arg_json(args, "args_json")? {
                    body["args"] = a;
                }
                let q = arg_str(args, "queue");
                if !q.is_empty() {
                    body["queue"] = json!(q);
                }
                if let Some(p) = arg_int(args, "priority") {
                    body["priority"] = json!(p);
                }
                self.call(R::post("/api/workers", body)).await
            }
            "moul_cancel_job" => {
                let id = need("id")?;
                self.call(R::patch(
                    &format!("/api/workers/{}", enc(&id)),
                    json!({"state": "cancelled"}),
                ))
                .await?;
                Ok(json!(format!("Job \"{id}\" cancelled successfully")))
            }
            "moul_list_feature_flags" => self.call(R::get("/api/feature-flags")).await,
            "moul_set_feature_flag" => {
                let key = need("key")?;
                let enabled = match &args["enabled"] {
                    Value::Bool(b) => *b,
                    Value::String(s) => s != "false" && s != "0",
                    _ => true,
                };
                let mut body = json!({"enabled": enabled});
                for k in ["description", "default_value"] {
                    let v = arg_str(args, k);
                    if !v.is_empty() {
                        body[k] = json!(v);
                    }
                }
                let path = format!("/api/feature-flags/{}", enc(&key));
                if self.call(R::get(&path)).await.is_ok() {
                    self.call(R::patch(&path, body)).await
                } else {
                    body["key"] = json!(key);
                    self.call(R::post("/api/feature-flags", body)).await
                }
            }
            "moul_get_system_metrics" => match self.metrics {
                Some((m, _)) => Ok(m.snapshot()),
                None => Err("system monitoring runs only on a server".into()),
            },
            "moul_get_analytics_summary" => {
                let counts = self
                    .db
                    .run(|db| {
                        let conn = db.engine.store().conn();
                        let n = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
                        json!({"total_requests": n("SELECT count(*) FROM writer._requests"), "total_visits": n("SELECT count(*) FROM writer._visits")})
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(counts)
            }
            "moul_list_requests" => {
                let limit = arg_int(args, "limit")
                    .filter(|l| (1..=500).contains(l))
                    .unwrap_or(30);
                self.db
                    .run(move |db| {
                        let conn = db.engine.store().conn();
                        let mut stmt = conn
                            .prepare(&format!(
                                "SELECT id, method, path, status_code, response_time_ms, createdAt FROM writer._requests ORDER BY createdAt DESC LIMIT {limit}"
                            ))
                            .map_err(|e| e.to_string())?;
                        let rows = stmt
                            .query_map([], |r| {
                                Ok(json!({
                                    "id": r.get::<_, String>(0)?, "method": r.get::<_, String>(1)?,
                                    "path": r.get::<_, String>(2)?, "status_code": r.get::<_, i64>(3)?,
                                    "response_time_ms": r.get::<_, i64>(4)?, "createdAt": r.get::<_, String>(5)?,
                                }))
                            })
                            .map_err(|e| e.to_string())?
                            .filter_map(Result::ok)
                            .collect::<Vec<_>>();
                        Ok(Value::Array(rows))
                    })
                    .await
                    .map_err(|e| e.to_string())?
            }
            "moul_get_schema" => self.call(R::get("/api/schema")).await,
            "moul_apply_schema" => {
                let schema = arg_json(args, "schema_json")?.ok_or("schema_json is required")?;
                let mut r = R::post("/api/schema", schema);
                if args["prune"] == true || args["prune"] == "true" {
                    r = r.query("prune", "true");
                }
                self.call(r).await
            }
            "moul_list_changes" => {
                let after = arg_int(args, "after").unwrap_or(0);
                let limit = arg_int(args, "limit")
                    .filter(|l| (1..=1000).contains(l))
                    .unwrap_or(50);
                self.call(
                    R::get("/api/changes")
                        .query("after", &after.to_string())
                        .query("limit", &limit.to_string()),
                )
                .await
            }
            "moul_query" => {
                let sql = need("sql")?;
                let (columns, rows) = self
                    .db
                    .run(move |db| db.engine.query_shared(&sql, 500))
                    .await
                    .map_err(|e| e.to_string())??;
                Ok(json!({"columns": columns, "rows": rows}))
            }
            other => Err(format!("tool '{other}' not found: tool not found")),
        }
    }

    /// Answers one JSON-RPC message; `None` for a notification.
    pub async fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("");
        let reply =
            |result: Value| Some(json!({"jsonrpc": "2.0", "id": id.clone(), "result": result}));
        let fail = |code: i64, message: &str| {
            Some(
                json!({"jsonrpc": "2.0", "id": id.clone(), "error": {"code": code, "message": message}}),
            )
        };
        if msg.get("jsonrpc") != Some(&json!("2.0")) || method.is_empty() {
            return id.as_ref().and(fail(-32600, "Invalid Request"));
        }
        // Notifications get no answer.
        id.as_ref()?;
        match method {
            "initialize" => {
                let asked = msg["params"]["protocolVersion"].as_str().unwrap_or("");
                let version = if asked.is_empty() {
                    "2025-03-26"
                } else if VERSIONS.contains(&asked) {
                    asked
                } else {
                    VERSIONS[0]
                };
                reply(json!({
                    "protocolVersion": version,
                    "capabilities": {"logging": {}, "tools": {"listChanged": true}},
                    "serverInfo": {"name": self.name, "version": env!("CARGO_PKG_VERSION")},
                }))
            }
            "ping" => reply(json!({})),
            "tools/list" => reply(json!({"tools": list()})),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                if !list().iter().any(|t| t["name"] == name) {
                    return fail(-32602, &format!("tool '{name}' not found: tool not found"));
                }
                let args = msg["params"]["arguments"].clone();
                reply(self.run(name, &args).await)
            }
            "logging/setLevel" => reply(json!({})),
            "resources/list" => reply(json!({"resources": []})),
            "prompts/list" => reply(json!({"prompts": []})),
            _ => fail(-32601, &format!("Method not found: {method}")),
        }
    }
}

fn plain(status: StatusCode, text: &str) -> Response {
    let mut r = Response::new(Body::from(text.to_owned()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    r
}

/// Whether a session id has the shape this server hands out.
fn valid_session(id: &str) -> bool {
    id.strip_prefix("mcp-session-")
        .is_some_and(|u| u.len() == 36 && u.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}

fn new_session() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the operating system supplies randomness");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = moul_core::ids::hex(&b);
    format!(
        "mcp-session-{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// `/api/mcp` and `/mcp`: Streamable HTTP. A `POST` carries one JSON-RPC
/// message (or a batch) and gets the answer as JSON. `initialize` hands out
/// an `Mcp-Session-Id`, which later requests carry. The server keeps no
/// session state and sends nothing unasked, so `GET` answers 405.
pub(crate) async fn http(State(state): State<AppState>, req: Request) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0);
    if let Err(r) = require_admin(&state, req.headers(), ip).await {
        return r;
    }
    match *req.method() {
        Method::POST => {}
        Method::DELETE => return Response::new(Body::empty()),
        Method::GET => {
            let mut r = plain(
                StatusCode::METHOD_NOT_ALLOWED,
                "This server sends no messages unasked\n",
            );
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
            return r;
        }
        _ => return plain(StatusCode::NOT_FOUND, "404 page not found\n"),
    }
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !ct.starts_with("application/json") {
        return plain(
            StatusCode::BAD_REQUEST,
            "Invalid content type: must be 'application/json'\n",
        );
    }
    let session = req
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(req.into_body(), 4 << 20)
        .await
        .unwrap_or_default();
    let msg: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            let body = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "request body is not valid json"}});
            let mut r = axum::Json(body).into_response_ok();
            *r.status_mut() = StatusCode::BAD_REQUEST;
            return r;
        }
    };
    let messages: Vec<Value> = match &msg {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let initializing = messages.iter().any(|m| m["method"] == "initialize");
    let new_id = if initializing {
        Some(new_session())
    } else {
        match &session {
            Some(s) if valid_session(s) => None,
            _ => return plain(StatusCode::NOT_FOUND, "Invalid session ID\n"),
        }
    };
    let tools = Tools {
        db: &state.db,
        name: state.app_name(),
        metrics: Some((&state.metrics, state.started)),
    };
    let mut replies = Vec::new();
    for m in &messages {
        if let Some(r) = tools.handle(m).await {
            replies.push(r);
        }
    }
    let mut resp = if replies.is_empty() {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::ACCEPTED;
        r
    } else {
        let body = if msg.is_array() {
            Value::Array(replies)
        } else {
            replies.remove(0)
        };
        let mut text = serde_json::to_vec(&body).unwrap_or_default();
        text.push(b'\n');
        let mut r = Response::new(Body::from(text));
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        r
    };
    if let Some(id) = new_id.and_then(|i| HeaderValue::try_from(i).ok()) {
        resp.headers_mut().insert("mcp-session-id", id);
    }
    resp
}

trait IntoResponseOk {
    fn into_response_ok(self) -> Response;
}

impl<T: axum::response::IntoResponse> IntoResponseOk for T {
    fn into_response_ok(self) -> Response {
        self.into_response()
    }
}

/// Serves MCP over stdin and stdout, one JSON-RPC message per line, until
/// stdin closes.
pub async fn serve_stdio(db: DbHandle, name: String) -> std::io::Result<()> {
    let tools = Tools {
        db: &db,
        name,
        metrics: None,
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let mut replies = Vec::new();
                for m in &batch {
                    if let Some(r) = tools.handle(m).await {
                        replies.push(r);
                    }
                }
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(m) => tools.handle(&m).await,
            Err(_) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}),
            ),
        };
        if let Some(r) = reply {
            let mut text = serde_json::to_vec(&r).unwrap_or_default();
            text.push(b'\n');
            out.write_all(&text).await?;
            out.flush().await?;
        }
    }
    Ok(())
}
