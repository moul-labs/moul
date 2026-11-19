//! Feature flags: port of the Go engine's flags, kept in the shared store's
//! `_feature_flags` so every device that follows sees them.
//!
//! A flag is on or off, with a default value and three gates checked in
//! order: actors (a user or other id), groups (from the context's `groups`
//! and `role`), and a percentage of targets, bucketed by CRC-32 of
//! `<key>:<target>` as Go does so a rollout keeps its members.
//!
//! | Path | Methods | Who |
//! | --- | --- | --- |
//! | `/api/feature-flags` | `GET`, `POST` | root users and the owner |
//! | `/api/feature-flags/:key` | `GET`, `PATCH`, `DELETE` | root users and the owner |
//! | `/api/feature-flags/:key/eval` | `POST` | anyone signed in; a member's identity is pinned to their token |
//!
//! Errors are `{"error": …}`, as in Go.

use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Response};
use crate::changes::ChangeInfo;
use crate::engine::{Cx, Who};

fn err(status: u16, message: &str) -> ApiResult {
    Ok(Response::json(status, json!({"error": message})))
}

/// A flag as stored.
#[derive(Clone, Debug, PartialEq)]
struct Flag {
    id: String,
    key: String,
    description: String,
    enabled: bool,
    default_value: String,
    gates: Value,
    created_at: String,
    updated_at: String,
}

impl Flag {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "key": self.key,
            "description": self.description,
            "enabled": self.enabled,
            "default_value": self.default_value,
            "gates": self.gates,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
        })
    }
}

const COLUMNS: &str = "id, key, description, enabled, default_value, gates, createdAt, updatedAt";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Flag> {
    let gates: String = r.get(5)?;
    Ok(Flag {
        id: r.get(0)?,
        key: r.get(1)?,
        description: r.get(2)?,
        enabled: r.get::<_, i64>(3)? != 0,
        default_value: r.get(4)?,
        gates: serde_json::from_str(&gates).unwrap_or_else(|_| json!({})),
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

fn load(cx: &Cx<'_>, key: &str) -> ApiResult<Option<Flag>> {
    Ok(cx
        .conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM main._feature_flags WHERE key = ?1"),
            [key],
            row,
        )
        .optional()?)
}

fn save(cx: &Cx<'_>, f: &Flag) -> ApiResult<()> {
    cx.conn.execute(
        "INSERT INTO main._feature_flags (id, key, description, enabled, default_value, gates, createdAt, updatedAt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (key) DO UPDATE SET description = excluded.description,
           enabled = excluded.enabled, default_value = excluded.default_value,
           gates = excluded.gates, updatedAt = excluded.updatedAt",
        params![
            f.id,
            f.key,
            f.description,
            f.enabled as i64,
            f.default_value,
            f.gates.to_string(),
            f.created_at,
            f.updated_at
        ],
    )?;
    Ok(())
}

fn logged(cx: &mut Cx<'_>, key: &str) {
    cx.change = Some(ChangeInfo {
        collection: None,
        record: Some(key.to_owned()),
        fields: Vec::new(),
        effects: Vec::new(),
    });
}

fn body(cx: &Cx<'_>) -> Result<Map<String, Value>, ()> {
    match cx.req.json_object() {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Ok(Map::new()),
        Err(()) => Err(()),
    }
}

/// Keeps only gates of the shapes Go reads: `actors` and `groups` as maps
/// of booleans, `percentage` as `{percentage, attribute?}`.
fn clean_gates(v: &Value) -> Result<Value, &'static str> {
    let Some(m) = v.as_object() else {
        return if v.is_null() {
            Ok(json!({}))
        } else {
            Err("gates must be an object")
        };
    };
    let mut out = Map::new();
    for name in ["actors", "groups"] {
        if let Some(g) = m.get(name).filter(|g| !g.is_null()) {
            let ok = g
                .as_object()
                .is_some_and(|o| o.values().all(Value::is_boolean));
            if !ok {
                return Err("gates.actors and gates.groups must map names to true or false");
            }
            out.insert(name.into(), g.clone());
        }
    }
    if let Some(p) = m.get("percentage").filter(|p| !p.is_null()) {
        let pct = p["percentage"].as_f64();
        if !p.is_object() || !pct.is_some_and(|x| (0.0..=100.0).contains(&x)) {
            return Err("gates.percentage.percentage must be a number from 0 to 100");
        }
        if !(p["attribute"].is_null() || p["attribute"].is_string()) {
            return Err("gates.percentage.attribute must be a string");
        }
        out.insert("percentage".into(), p.clone());
    }
    Ok(Value::Object(out))
}

/// `GET /api/feature-flags`, by key.
pub(crate) fn list(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let mut stmt = cx.conn.prepare(&format!(
        "SELECT {COLUMNS} FROM main._feature_flags ORDER BY key"
    ))?;
    let flags = stmt
        .query_map([], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Response::json(
        200,
        Value::Array(flags.iter().map(Flag::to_json).collect()),
    ))
}

/// `GET /api/feature-flags/:key`
pub(crate) fn get(cx: &mut Cx<'_>, key: &str) -> ApiResult {
    cx.require_admin()?;
    match load(cx, key)? {
        Some(f) => Ok(Response::json(200, f.to_json())),
        None => err(404, "Flag not found"),
    }
}

/// `POST /api/feature-flags`
pub(crate) fn create(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let Ok(b) = body(cx) else {
        return err(400, "Invalid request payload");
    };
    let key = b
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if key.is_empty() {
        return err(400, "Flag key is required");
    }
    // Go updated the flag of that key and answered as if it made a new one.
    if load(cx, &key)?.is_some() {
        return err(400, "A flag with this key already exists");
    }
    let gates = match clean_gates(b.get("gates").unwrap_or(&Value::Null)) {
        Ok(g) => g,
        Err(m) => return err(400, m),
    };
    let mut default_value = text_value(b.get("default_value"));
    if default_value.is_empty() {
        default_value = "false".into();
    }
    let mut n = [0u8; 4];
    cx.host.fill_random(&mut n);
    let now = cx.now();
    let f = Flag {
        // Go's `ff_<unix nanoseconds>`.
        id: format!(
            "ff_{}",
            cx.now_ms * 1_000_000 + i64::from(u32::from_le_bytes(n) % 1_000_000)
        ),
        key: key.clone(),
        description: text_value(b.get("description")),
        enabled: b.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        default_value,
        gates,
        created_at: now.clone(),
        updated_at: now,
    };
    save(cx, &f)?;
    logged(cx, &key);
    Ok(Response::json(201, f.to_json()))
}

/// A setting given as a string, or any other JSON written out.
fn text_value(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// `PATCH /api/feature-flags/:key`
pub(crate) fn update(cx: &mut Cx<'_>, key: &str) -> ApiResult {
    cx.require_admin()?;
    let Ok(b) = body(cx) else {
        return err(400, "Invalid request payload");
    };
    let Some(mut f) = load(cx, key)? else {
        return err(404, "Flag not found");
    };
    if let Some(d) = b.get("description").filter(|v| !v.is_null()) {
        f.description = text_value(Some(d));
    }
    if let Some(e) = b.get("enabled").and_then(Value::as_bool) {
        f.enabled = e;
    }
    if let Some(d) = b.get("default_value").filter(|v| !v.is_null()) {
        f.default_value = text_value(Some(d));
    }
    if let Some(g) = b.get("gates") {
        f.gates = match clean_gates(g) {
            Ok(g) => g,
            Err(m) => return err(400, m),
        };
    }
    f.updated_at = cx.now();
    save(cx, &f)?;
    logged(cx, key);
    Ok(Response::json(200, f.to_json()))
}

/// `DELETE /api/feature-flags/:key`, also for a key that does not exist.
pub(crate) fn delete(cx: &mut Cx<'_>, key: &str) -> ApiResult {
    cx.require_admin()?;
    let n = cx
        .conn
        .execute("DELETE FROM main._feature_flags WHERE key = ?1", [key])?;
    if n > 0 {
        logged(cx, key);
    }
    Ok(Response::json(
        200,
        json!({"message": "Flag deleted successfully"}),
    ))
}

/// `POST /api/feature-flags/:key/eval` with `{"context": {…}}`.
pub(crate) fn eval(cx: &mut Cx<'_>, key: &str) -> ApiResult {
    if !cx.who.is_admin() && cx.who.user().is_none() {
        return Err(ApiError::unauthorized(
            "Authentication required (either valid JWT user or admin key)",
        ));
    }
    let Ok(b) = body(cx) else {
        return err(400, "Invalid request payload");
    };
    let Some(f) = load(cx, key)? else {
        return err(404, "Flag not found");
    };
    let mut context = b
        .get("context")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // A member may ask only about themself.
    if let Who::User(u) = &cx.who {
        for k in ["target_id", "user_id", "actor_id", "actor", "user", "id"] {
            context.insert(k.into(), json!(u.id));
        }
        context.insert("email".into(), json!(u.email));
        context.remove("org_id");
        if let Some(attr) = f.gates["percentage"]["attribute"]
            .as_str()
            .filter(|a| !a.is_empty())
        {
            context.insert(attr.into(), json!(u.id));
        }
    }
    let (value, reason) = evaluate(&f, &context);
    Ok(Response::json(
        200,
        json!({"value": value, "reason": reason}),
    ))
}

/// Evaluates `f` for `ctx`: the value and the reason, as Go's `Evaluate`.
fn evaluate(f: &Flag, ctx: &Map<String, Value>) -> (Value, &'static str) {
    if !f.enabled {
        return (parse_value(&f.default_value), "DISABLED");
    }
    if let Some(actors) = f.gates["actors"].as_object().filter(|a| !a.is_empty()) {
        for k in actor_keys(ctx) {
            if let Some(v) = actors.get(&k).and_then(Value::as_bool) {
                return (json!(v), "TARGETING_MATCH (Actor)");
            }
        }
    }
    if let Some(groups) = f.gates["groups"].as_object().filter(|g| !g.is_empty()) {
        for g in context_groups(ctx) {
            if let Some(v) = groups.get(&g).and_then(Value::as_bool) {
                return (json!(v), "TARGETING_MATCH (Group)");
            }
        }
    }
    let pct = f.gates["percentage"]["percentage"].as_f64().unwrap_or(0.0);
    if pct > 0.0 {
        let attr = f.gates["percentage"]["attribute"].as_str().unwrap_or("");
        let target = target_id(ctx, attr);
        if !target.is_empty() {
            let bucket =
                f64::from(crc32(format!("{}:{target}", f.key).as_bytes()) % 10_000) / 100.0;
            return (json!(bucket < pct), "TARGETING_MATCH (Percentage)");
        }
    }
    (parse_value(&f.default_value), "DEFAULT")
}

/// A context value as Go's `fmt.Sprintf("%v")` writes it.
fn go_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map(go_float).unwrap_or_else(|| n.to_string()),
        Value::Null => "<nil>".into(),
        // Go writes maps and slices as `map[...]` and `[...]`; nothing
        // useful matches those, so any stable text will do.
        other => other.to_string(),
    }
}

/// A float as Go's `%v` (that is `%g` with the shortest digits) writes it:
/// exponent form for exponents from 6 up or below -4, so `1e+06`.
fn go_float(f: f64) -> String {
    if f == 0.0 {
        return "0".into();
    }
    if !f.is_finite() {
        return if f.is_nan() {
            "NaN".into()
        } else if f > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        };
    }
    // Shortest round-trip digits and the decimal exponent.
    let e = format!("{f:e}");
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    format!("{f}")
}

fn actor_keys(ctx: &Map<String, Value>) -> Vec<String> {
    let mut keys = Vec::new();
    for field in [
        "target_id",
        "user_id",
        "actor_id",
        "actor",
        "user",
        "email",
        "org_id",
    ] {
        let Some(v) = ctx.get(field).filter(|v| !v.is_null()) else {
            continue;
        };
        let s = go_string(v);
        if s.is_empty() {
            continue;
        }
        keys.push(s.clone());
        keys.push(format!("{field}:{s}"));
        if let Some(short) = field.strip_suffix("_id") {
            keys.push(format!("{short}:{s}"));
        }
    }
    keys
}

fn context_groups(ctx: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    match ctx.get("groups") {
        Some(Value::Array(a)) => out.extend(a.iter().map(go_string)),
        Some(Value::String(s)) => out.push(s.clone()),
        _ => {}
    }
    if let Some(r) = ctx.get("role").filter(|v| !v.is_null()) {
        out.push(go_string(r));
    }
    out
}

fn target_id(ctx: &Map<String, Value>, attr: &str) -> String {
    if !attr.is_empty()
        && let Some(v) = ctx.get(attr).filter(|v| !v.is_null())
    {
        return go_string(v);
    }
    for field in ["target_id", "user_id", "actor_id", "id", "email"] {
        if let Some(v) = ctx.get(field).filter(|v| !v.is_null()) {
            let s = go_string(v);
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}

/// A default value: `true` or `false` in any case, else JSON, else the text.
fn parse_value(v: &str) -> Value {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" => return json!(true),
        "false" => return json!(false),
        _ => {}
    }
    serde_json::from_str(v).unwrap_or_else(|_| json!(v))
}

/// CRC-32 (IEEE), as Go's `crc32.ChecksumIEEE`.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(gates: Value) -> Flag {
        Flag {
            id: "ff_1".into(),
            key: "new-billing-flow".into(),
            description: String::new(),
            enabled: true,
            default_value: "false".into(),
            gates,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn crc32_matches_go() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn numbers_print_as_go_does() {
        assert_eq!(go_float(101.0), "101");
        assert_eq!(go_float(1.5), "1.5");
        assert_eq!(go_float(123_456.0), "123456");
        assert_eq!(go_float(1_000_000.0), "1e+06");
        assert_eq!(go_float(1_234_567.0), "1.234567e+06");
        assert_eq!(go_float(1e21), "1e+21");
        assert_eq!(go_float(0.00001), "1e-05");
    }

    #[test]
    fn gates_match_in_order() {
        let f = flag(json!({"actors": {"user:101": true}, "groups": {"beta": true}}));
        let ctx = |v: Value| v.as_object().cloned().unwrap();
        assert_eq!(
            evaluate(&f, &ctx(json!({"user_id": "101"}))),
            (json!(true), "TARGETING_MATCH (Actor)")
        );
        assert_eq!(
            evaluate(&f, &ctx(json!({"user_id": 101}))),
            (json!(true), "TARGETING_MATCH (Actor)")
        );
        assert_eq!(
            evaluate(&f, &ctx(json!({"role": "beta"}))),
            (json!(true), "TARGETING_MATCH (Group)")
        );
        assert_eq!(evaluate(&f, &ctx(json!({}))), (json!(false), "DEFAULT"));
        let all = flag(json!({"percentage": {"percentage": 100}}));
        assert_eq!(
            evaluate(&all, &ctx(json!({"id": "x"}))),
            (json!(true), "TARGETING_MATCH (Percentage)")
        );
        let mut off = flag(json!({}));
        off.enabled = false;
        off.default_value = "{\"a\":1}".into();
        assert_eq!(
            evaluate(&off, &ctx(json!({}))),
            (json!({"a": 1}), "DISABLED")
        );
    }
}
