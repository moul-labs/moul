//! Webhooks: a collection's list of URLs to call when its records change.
//!
//! The list lives in the collection's definition (`_moul.webhooks`), as in
//! the Go engine; secrets live in the writer-only store. A write that
//! changes records adds one row to `writer._outbox` for each webhook that
//! wants the event, in the write's own transaction, so a call is owed
//! exactly when the change is kept. The native server sends them
//! ([`crate::effects`]); a tab only keeps them.
//!
//! | Path | Methods | Who |
//! | --- | --- | --- |
//! | `/api/moul/:name/webhooks` | `GET`, `POST` | root users and the owner |
//! | `/api/moul/:name/webhooks/:id` | `GET`, `PATCH`, `PUT`, `DELETE` | root users and the owner |
//! | `/api/moul/:name/webhooks/test`, `/:id/test` | `POST` | answered by the native server, which can call out |
//!
//! Where this departs from Go:
//!
//! - Only `:after` events fire (`create`, `update`, `delete`, `*`, `all`
//!   and `<action>:after` select them). A `:before` webhook would make every
//!   write wait on someone else's server; one is refused when saved.
//! - Calls are rows, retried with backoff until delivered, and never sent
//!   twice by followers. Go sent each once, from memory, and dropped it on a
//!   restart or a failure.
//! - A new webhook is enabled unless it says otherwise, and a change that
//!   does not mention `enabled` keeps it (Go disabled both).
//! - Secrets are written, never read back: answers show `"secret": "********"`
//!   when one is set.

use moul_store::catalog;
use moul_store::schema::Collection;
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Response};
use crate::engine::Cx;
use crate::host::Host;
use crate::realtime::{Action, RecordEvent};
use crate::{ids, time, values};

/// What a webhook's secret reads as.
pub const MASK: &str = "********";

fn not_found() -> ApiError {
    ApiError::not_found("Not found")
}

fn collection(cx: &mut Cx<'_>, name: &str) -> ApiResult<Collection> {
    cx.require_admin()?;
    cx.collection(name)?.ok_or_else(not_found)
}

fn has_secret(conn: &Connection, collection_id: &str, webhook: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM writer._webhook_secrets WHERE collection = ?1 AND webhook = ?2 AND secret != ''",
        params![collection_id, webhook],
        |_| Ok(()),
    )
    .is_ok()
}

/// The secret of a webhook, if one is set.
pub(crate) fn secret(conn: &Connection, collection_id: &str, webhook: &str) -> Option<String> {
    conn.query_row(
        "SELECT secret FROM writer._webhook_secrets WHERE collection = ?1 AND webhook = ?2",
        params![collection_id, webhook],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .filter(|s| !s.is_empty())
}

/// A webhook as an answer shows it.
fn shown(conn: &Connection, c: &Collection, hook: &Value) -> Value {
    let mut h = hook.as_object().cloned().unwrap_or_default();
    h.retain(|k, _| !k.eq_ignore_ascii_case("secret"));
    let id = h.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
    if has_secret(conn, &c.id, &id) {
        h.insert("secret".into(), json!(MASK));
    }
    Value::Object(h)
}

fn hook_id(h: &Value) -> &str {
    h.get("id").and_then(Value::as_str).unwrap_or("")
}

/// Checks the events a webhook asks for.
fn check_events(events: &[String]) -> ApiResult<()> {
    for e in events {
        let e = e.trim().to_ascii_lowercase();
        if e.ends_with(":before") {
            return Err(ApiError::bad_request(format!(
                "{e}: webhooks that run before a write are not supported; use {} instead",
                e.replace(":before", ":after")
            )));
        }
        let ok = matches!(
            e.as_str(),
            "*" | "all"
                | "create"
                | "update"
                | "delete"
                | "create:after"
                | "update:after"
                | "delete:after"
        );
        if !ok {
            return Err(ApiError::bad_request(format!(
                "{e:?} is not an event: use create, update, delete, *, or one of them with :after"
            )));
        }
    }
    Ok(())
}

fn events_of(v: Option<&Value>) -> ApiResult<Option<Vec<String>>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            let mut out = Vec::new();
            for e in a {
                let s = e
                    .as_str()
                    .ok_or_else(|| ApiError::bad_request("events must be a list of strings"))?;
                out.push(s.trim().to_owned());
            }
            out.retain(|s| !s.is_empty());
            Ok(Some(out))
        }
        Some(_) => Err(ApiError::bad_request("events must be a list of strings")),
    }
}

/// Checks a webhook URL: absolute `http` or `https`. Whether its address is
/// public is checked when it is called, since names can change.
fn check_url(url: &str) -> ApiResult<()> {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    match rest {
        Some(r)
            if !r.is_empty() && !r.starts_with('/') && !url.chars().any(char::is_whitespace) =>
        {
            Ok(())
        }
        _ => Err(ApiError::bad_request(
            "Webhook URL must be an absolute http or https URL",
        )),
    }
}

fn save(cx: &mut Cx<'_>, mut c: Collection, hooks: Vec<Value>) -> ApiResult<()> {
    c.webhooks = hooks;
    c.updated_at = cx.now();
    catalog::save(cx.conn, &c)?;
    cx.changed(&c, None, Vec::new());
    Ok(())
}

fn body(cx: &Cx<'_>) -> ApiResult<Map<String, Value>> {
    match cx.req.json_object() {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Ok(Map::new()),
        Err(()) => Err(ApiError::bad_request("Invalid request body")),
    }
}

/// `GET /api/moul/:name/webhooks`
pub(crate) fn list(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = collection(cx, name)?;
    let hooks: Vec<Value> = c.webhooks.iter().map(|h| shown(cx.conn, &c, h)).collect();
    Ok(Response::json(200, Value::Array(hooks)))
}

/// `GET /api/moul/:name/webhooks/:id`
pub(crate) fn get(cx: &mut Cx<'_>, name: &str, id: &str) -> ApiResult {
    let c = collection(cx, name)?;
    let hook = c
        .webhooks
        .iter()
        .find(|h| hook_id(h) == id)
        .ok_or_else(|| ApiError::not_found("Webhook not found"))?;
    Ok(Response::json(200, shown(cx.conn, &c, hook)))
}

/// `POST /api/moul/:name/webhooks`
pub(crate) fn create(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = collection(cx, name)?;
    let b = body(cx)?;
    let url = b
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if url.is_empty() {
        return Err(ApiError::bad_request("Webhook URL is required"));
    }
    check_url(&url)?;
    let mut events = events_of(b.get("events"))?.unwrap_or_default();
    if events.is_empty() {
        events = vec!["*".into()];
    }
    check_events(&events)?;
    let id = match b.get("id").and_then(Value::as_str).map(str::trim) {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => format!("wh-{}", ids::random_chars(cx.host, 15)),
    };
    if c.webhooks.iter().any(|h| hook_id(h) == id) {
        return Err(ApiError::bad_request(
            "A webhook with this id already exists",
        ));
    }
    let now = cx.now();
    let mut hook = json!({
        "id": id,
        "url": url,
        "events": events,
        "enabled": b.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        "createdAt": now,
        "updatedAt": now,
    });
    if let Some(s) = b
        .get("secret")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && *s != MASK)
    {
        hook["secret"] = json!(s);
    }
    let mut hooks = c.webhooks.clone();
    hooks.push(hook.clone());
    save(cx, c.clone(), hooks)?;
    Ok(Response::json(201, shown(cx.conn, &c, &hook)))
}

/// `PATCH|PUT /api/moul/:name/webhooks/:id`: changes what the body names.
/// An empty `secret` removes the secret.
pub(crate) fn update(cx: &mut Cx<'_>, name: &str, id: &str) -> ApiResult {
    let c = collection(cx, name)?;
    let b = body(cx)?;
    let mut hooks = c.webhooks.clone();
    let hook = hooks
        .iter_mut()
        .find(|h| hook_id(h) == id)
        .ok_or_else(|| ApiError::not_found("Webhook not found"))?;
    if let Some(url) = b
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        check_url(url)?;
        hook["url"] = json!(url);
    }
    if let Some(events) = events_of(b.get("events"))?.filter(|e| !e.is_empty()) {
        check_events(&events)?;
        hook["events"] = json!(events);
    }
    if let Some(e) = b.get("enabled").and_then(Value::as_bool) {
        hook["enabled"] = json!(e);
    }
    match b.get("secret").and_then(Value::as_str) {
        Some("") => {
            cx.conn.execute(
                "DELETE FROM writer._webhook_secrets WHERE collection = ?1 AND webhook = ?2",
                params![c.id, id],
            )?;
        }
        Some(s) if s != MASK => hook["secret"] = json!(s),
        _ => {}
    }
    hook["updatedAt"] = json!(cx.now());
    let hook = hook.clone();
    save(cx, c.clone(), hooks)?;
    Ok(Response::json(200, shown(cx.conn, &c, &hook)))
}

/// `DELETE /api/moul/:name/webhooks/:id`
pub(crate) fn delete(cx: &mut Cx<'_>, name: &str, id: &str) -> ApiResult {
    let c = collection(cx, name)?;
    if !c.webhooks.iter().any(|h| hook_id(h) == id) {
        return Err(ApiError::not_found("Webhook not found"));
    }
    let hooks: Vec<Value> = c
        .webhooks
        .iter()
        .filter(|h| hook_id(h) != id)
        .cloned()
        .collect();
    cx.conn.execute(
        "DELETE FROM writer._webhook_secrets WHERE collection = ?1 AND webhook = ?2",
        params![c.id, id],
    )?;
    save(cx, c, hooks)?;
    Ok(Response::empty(204))
}

/// `POST /api/moul/:name/webhooks/test` in a tab, which cannot call out.
pub(crate) fn test(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    collection(cx, name)?;
    Err(ApiError::new(
        501,
        "Testing a webhook needs the native server, which can call out",
    ))
}

/// Whether a webhook asks for `event` (`create:after`, …).
fn wants(hook: &Value, event: &str) -> bool {
    if !hook["enabled"].as_bool().unwrap_or(false) {
        return false;
    }
    let action = event.split(':').next().unwrap_or("");
    hook["events"].as_array().is_some_and(|evs| {
        evs.iter().filter_map(Value::as_str).any(|e| {
            let e = e.trim().to_ascii_lowercase();
            e == "*" || e == "all" || e == event || e == action
        })
    })
}

/// Queues the webhook calls `events` owe, in the caller's transaction.
pub(crate) fn queue(
    conn: &Connection,
    host: &mut dyn Host,
    events: &[RecordEvent],
    now_ms: i64,
) -> rusqlite::Result<()> {
    let now = time::rfc3339(now_ms);
    for ev in events {
        let c = &ev.collection;
        if c.webhooks.is_empty() {
            continue;
        }
        let action = match ev.action {
            Action::Create => "create",
            Action::Update => "update",
            Action::Delete => "delete",
        };
        let event = format!("{action}:after");
        let mut record = ev.record.clone();
        values::normalize(c, &mut record);
        let old = match ev.action {
            Action::Delete => Some(record.clone()),
            _ => ev.old.clone().map(|mut o| {
                values::normalize(c, &mut o);
                o
            }),
        };
        for hook in c.webhooks.iter().filter(|h| wants(h, &event)) {
            let Some(url) = hook["url"].as_str() else {
                continue;
            };
            let mut payload = json!({
                "event": event,
                "moul": c.name,
                "record": record,
                "timestamp": now,
            });
            if let Some(o) = &old {
                payload["old_record"] = Value::Object(o.clone());
            }
            conn.execute(
                "INSERT INTO writer._outbox (id, kind, collection, target, url, event, payload, nextAt, createdAt)
                 VALUES (?1, 'webhook', ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![
                    format!("out_{}", ids::random_chars(host, 15)),
                    c.id,
                    hook_id(hook),
                    url,
                    event,
                    payload.to_string(),
                    now
                ],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_select_after_calls() {
        let h = json!({"enabled": true, "events": ["create"]});
        assert!(wants(&h, "create:after"));
        assert!(!wants(&h, "delete:after"));
        let all = json!({"enabled": true, "events": ["*"]});
        assert!(wants(&all, "update:after"));
        let off = json!({"enabled": false, "events": ["*"]});
        assert!(!wants(&off, "update:after"));
        assert!(check_events(&["create:before".into()]).is_err());
        assert!(check_events(&["created".into()]).is_err());
        assert!(check_events(&["UPDATE:after".into(), "all".into()]).is_ok());
        assert!(check_url("ftp://x").is_err());
        assert!(check_url("https://hooks.example.com/x").is_ok());
    }
}
