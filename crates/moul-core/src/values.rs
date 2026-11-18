//! Field values: checking what a request sends, storing it, and reading it
//! back as JSON. The rules and messages are the Go engine's
//! (`validateFieldConstraints`, the create and update paths, and
//! `normalizeRecord`).

use moul_store::schema::{Collection, CollectionKind, Field, FieldType};
use rusqlite::types::{Value as Sql, ValueRef};
use serde_json::{Map, Value};

use crate::api::ApiError;
use crate::time;

/// A record as JSON.
pub type Record = Map<String, Value>;

/// A number as the Go engine writes it: whole numbers without a fraction.
pub fn number(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// One SQLite value as JSON, before the field types are applied.
pub fn sql_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => number(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(crate::crypto::base64(b)),
    }
}

/// Reads a row of `SELECT *` into a record, without field types applied.
pub fn read_row(row: &rusqlite::Row<'_>, columns: &[String]) -> rusqlite::Result<Record> {
    let mut rec = Map::new();
    for (i, name) in columns.iter().enumerate() {
        rec.insert(name.clone(), sql_to_json(row.get_ref(i)?));
    }
    Ok(rec)
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Applies field types to a record read from its table: numbers, booleans,
/// JSON, files and many-to-many relations become JSON values; credentials
/// never appear (they are not in the table). Port of Go's `normalizeRecord`.
pub fn normalize(c: &Collection, rec: &mut Record) {
    for f in &c.fields {
        let many = f.relation.as_ref().is_some_and(|r| r.is_many());
        let Some(val) = rec.get(&f.name).cloned() else {
            if f.kind == FieldType::Relation && many {
                rec.insert(f.name.clone(), Value::Array(vec![]));
            }
            continue;
        };
        if val.is_null() {
            if f.kind == FieldType::Relation && many {
                rec.insert(f.name.clone(), Value::Array(vec![]));
            }
            continue;
        }
        match f.kind {
            FieldType::Number => {
                if let Some(n) = as_text(&val).and_then(|s| s.trim().parse::<f64>().ok()) {
                    rec.insert(f.name.clone(), number(n));
                }
            }
            FieldType::Bool => {
                let s = as_text(&val).unwrap_or_default();
                rec.insert(
                    f.name.clone(),
                    Value::Bool(s == "1" || s.eq_ignore_ascii_case("true")),
                );
            }
            FieldType::Json | FieldType::File => {
                if let Value::String(s) = &val
                    && !s.is_empty()
                    && let Ok(decoded) = serde_json::from_str::<Value>(s)
                {
                    rec.insert(f.name.clone(), decoded);
                }
            }
            FieldType::Relation if many => {
                let ids = match &val {
                    Value::String(s) if !s.is_empty() && s != "null" => {
                        match serde_json::from_str::<Vec<String>>(s) {
                            Ok(ids) => ids.into_iter().map(Value::String).collect(),
                            Err(_) => vec![],
                        }
                    }
                    _ => vec![],
                };
                rec.insert(f.name.clone(), Value::Array(ids));
            }
            FieldType::Cloak => {
                // Encrypted values cannot be read yet; show them masked.
                rec.insert(f.name.clone(), Value::String("••••".into()));
            }
            _ => {}
        }
    }
    match c.kind {
        CollectionKind::Worker => {
            for key in ["args", "meta", "tags", "errors"] {
                if let Some(Value::String(s)) = rec.get(key)
                    && !s.is_empty()
                    && let Ok(decoded) = serde_json::from_str::<Value>(s)
                {
                    rec.insert(key.into(), decoded);
                }
            }
        }
        CollectionKind::Analytic => {
            if let Some(Value::String(s)) = rec.get("properties")
                && !s.is_empty()
                && let Ok(decoded) = serde_json::from_str::<Value>(s)
            {
                rec.insert("properties".into(), decoded);
            }
        }
        _ => {}
    }
    // Cloak blind indexes never leave the engine.
    for f in &c.fields {
        if f.kind == FieldType::Cloak {
            rec.remove(&format!("{}Hash", f.name));
        }
    }
}

fn bad(msg: String) -> ApiError {
    ApiError::bad_request(msg)
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => go_parse_float(s),
        _ => None,
    }
}

/// Go's `strconv.ParseFloat`: decimal and hexadecimal (`0x1p4`) forms,
/// digits separated by underscores, and `Inf`, `Infinity` and `NaN` in any
/// case. The first check of a number field uses it, so values Go let
/// through to the stricter decimal check get the same message here.
pub fn go_parse_float(s: &str) -> Option<f64> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    let lower = body.to_ascii_lowercase();
    let special = match lower.as_str() {
        "inf" | "infinity" => Some(f64::INFINITY),
        "nan" => Some(f64::NAN),
        _ => None,
    };
    if let Some(v) = special {
        return Some(if neg { -v } else { v });
    }
    // Underscores may only sit between digits (after a base prefix too).
    let underscore_ok = |t: &str| {
        let b = t.as_bytes();
        let start = if lower.starts_with("0x") { 2 } else { 0 };
        b.iter().enumerate().all(|(i, &c)| {
            c != b'_'
                || (i > 0
                    && i + 1 < b.len()
                    && (b[i - 1].is_ascii_hexdigit() || i == start)
                    && b[i + 1].is_ascii_hexdigit())
        })
    };
    if !underscore_ok(body) {
        return None;
    }
    let clean: String = lower.chars().filter(|&c| c != '_').collect();
    let v = if let Some(hex) = clean.strip_prefix("0x") {
        let (mant, exp) = hex.split_once('p')?;
        // Clamp: past ±2000 the value is already 0 or infinite.
        let exp = exp.parse::<i64>().ok()?.clamp(-4000, 4000);
        let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        let mut m = 0f64;
        for c in int.chars().chain(frac.chars()) {
            m = m * 16.0 + c.to_digit(16)? as f64;
        }
        let shift = (exp - 4 * frac.len().min(4000) as i64).clamp(-4000, 4000) as i32;
        m * 2f64.powi(shift)
    } else {
        if clean.is_empty()
            || !clean
                .bytes()
                .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'+' | b'-'))
        {
            return None;
        }
        clean.parse::<f64>().ok()?
    };
    // Go refuses a finite spelling that overflows (ErrRange).
    if v.is_infinite() {
        return None;
    }
    Some(if neg { -v } else { v })
}

/// A float as Go's `%v` prints it: the shortest digits, in exponent form
/// (`1e+06`, `1.5e-05`) when the decimal exponent is below -4 or at least
/// 6, as `strconv` decides for the shortest form.
pub fn go_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp.parse().expect("an integer exponent");
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.unsigned_abs())
    } else {
        format!("{f}")
    }
}

fn fmt_bound(f: f64) -> String {
    go_float(f)
}

/// Checks a value against its field's constraints. Port of Go's
/// `validateFieldConstraints`; `None` is a missing or null value.
pub fn check_constraints(f: &Field, val: Option<&Value>) -> Result<(), ApiError> {
    let val = val.filter(|v| !v.is_null());
    if f.required {
        match val {
            None => return Err(bad(format!("Field {:?} is required", f.name))),
            Some(Value::String(s)) if s.trim().is_empty() => {
                return Err(bad(format!(
                    "Field {:?} is required and cannot be empty",
                    f.name
                )));
            }
            _ => {}
        }
    }
    let Some(val) = val else { return Ok(()) };
    match f.kind {
        FieldType::Number => {
            let Some(n) = as_f64(val) else {
                return Err(bad(format!("Field {:?} must be a number", f.name)));
            };
            if let Some(min) = f.min
                && n < min
            {
                return Err(bad(format!(
                    "Field {:?} must be at least {}",
                    f.name,
                    fmt_bound(min)
                )));
            }
            if let Some(max) = f.max
                && n > max
            {
                return Err(bad(format!(
                    "Field {:?} must be at most {}",
                    f.name,
                    fmt_bound(max)
                )));
            }
        }
        FieldType::Text => {
            if let Value::String(s) = val {
                let len = s.chars().count() as f64;
                if let Some(min) = f.min
                    && len < min
                {
                    return Err(bad(format!(
                        "Field {:?} length must be at least {} characters",
                        f.name,
                        fmt_bound(min)
                    )));
                }
                if let Some(max) = f.max
                    && len > max
                {
                    return Err(bad(format!(
                        "Field {:?} length must be at most {} characters",
                        f.name,
                        fmt_bound(max)
                    )));
                }
            }
        }
        FieldType::Bool => match val {
            Value::Bool(_) | Value::Number(_) => {}
            Value::String(s) => {
                let lower = s.trim().to_ascii_lowercase();
                if !["true", "false", "1", "0"].contains(&lower.as_str()) {
                    return Err(bad(format!("Field {:?} must be a boolean", f.name)));
                }
            }
            _ => return Err(bad(format!("Field {:?} must be a boolean", f.name))),
        },
        FieldType::Date => {
            let Value::String(s) = val else {
                return Err(bad(format!(
                    "Field {:?} must be a date string (YYYY-MM-DD)",
                    f.name
                )));
            };
            if !s.is_empty() && time::parse_date(s).is_none() {
                return Err(bad(format!(
                    "Field {:?} must be a valid date in YYYY-MM-DD format",
                    f.name
                )));
            }
        }
        FieldType::Datetime => {
            let Value::String(s) = val else {
                return Err(bad(format!(
                    "Field {:?} must be an ISO 8601 date-time string",
                    f.name
                )));
            };
            if !s.is_empty() && time::parse_datetime(s).is_none() {
                return Err(bad(format!(
                    "Field {:?} must be a valid ISO 8601 date-time string (e.g. 2026-08-12T10:15:44Z)",
                    f.name
                )));
            }
        }
        FieldType::Json => {
            if let Value::String(s) = val
                && !s.is_empty()
                && serde_json::from_str::<Value>(s).is_err()
            {
                return Err(bad(format!("Field {:?} must be valid JSON", f.name)));
            }
        }
        FieldType::Url => {
            let Value::String(s) = val else {
                return Err(bad(format!("Field {:?} must be a URL string", f.name)));
            };
            if !s.is_empty() && !is_http_url(s) {
                return Err(bad(format!(
                    "Field {:?} must be a valid HTTP/HTTPS URL",
                    f.name
                )));
            }
        }
        FieldType::Email => {
            let Value::String(s) = val else {
                return Err(bad(format!("Field {:?} must be an email string", f.name)));
            };
            if !s.is_empty() && (!s.contains('@') || !s.contains('.')) {
                return Err(bad(format!(
                    "Field {:?} must be a valid email address",
                    f.name
                )));
            }
        }
        FieldType::Select if !f.options.is_empty() => {
            let Value::String(s) = val else {
                return Err(bad(format!(
                    "Field {:?} must be a valid option string",
                    f.name
                )));
            };
            if !s.is_empty() && !f.options.iter().any(|o| o == s) {
                return Err(bad(format!(
                    "Field {:?} must be one of: {}",
                    f.name,
                    f.options.join(", ")
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether `s` is an absolute `http` or `https` URL with a host, as Go's
/// `url.ParseRequestURI` plus the engine's scheme and host checks accept:
/// no control characters, a valid user part, a host without spaces, a
/// numeric port, a closed IPv6 bracket, and valid escapes in the path.
fn is_http_url(s: &str) -> bool {
    if s.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return false;
    }
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    // The query is cut first and not checked; the authority runs to the
    // first '/'. A '#' is part of the path for a request URI.
    let rest = rest.split_once('?').map_or(rest, |(r, _)| r);
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = match authority.rsplit_once('@') {
        Some((user, host)) => {
            let user_ok = user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(&b));
            if !user_ok || !escapes_ok(user.as_bytes(), |_| true) {
                return false;
            }
            host
        }
        None => authority,
    };
    // Go's check is on the whole host, port included.
    if host.is_empty() {
        return false;
    }
    let valid_port = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if let Some(inner) = host.strip_prefix('[') {
        let Some((addr, after)) = inner.split_once(']') else {
            return false;
        };
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(valid_port)) {
            return false;
        }
        // An IPv6 address, with an optional zone written "%25…".
        let (ip, zone) = match addr.find("%25") {
            Some(z) => (&addr[..z], Some(&addr[z + 3..])),
            None => (addr, None),
        };
        if ip.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        if zone.is_some_and(|z| !escapes_ok(z.as_bytes(), |_| true)) {
            return false;
        }
    } else {
        let name = match host.rsplit_once(':') {
            Some((name, port)) if valid_port(port) => name,
            Some(_) => return false,
            None => host,
        };
        // Go's host rules: unreserved and sub-delimiter ASCII, any byte of
        // a UTF-8 name, and escapes of non-ASCII bytes (or "%25").
        let ascii_ok = |b: u8| {
            b >= 0x80
                || b.is_ascii_alphanumeric()
                || b"-._~!$&'()*+,;=\"<>".contains(&b)
                || b == b'%'
        };
        if !name.bytes().all(ascii_ok) {
            return false;
        }
        if !escapes_ok(name.as_bytes(), |byte| byte >= 0x80 || byte == b'%') {
            return false;
        }
    }
    escapes_ok(path.as_bytes(), |_| true)
}

/// Whether every '%' in `b` starts a complete escape whose byte `allowed`
/// accepts.
fn escapes_ok(b: &[u8], allowed: impl Fn(u8) -> bool) -> bool {
    let hex = |c: u8| (c as char).to_digit(16);
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            match (
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
            ) {
                (Some(h), Some(l)) if allowed((h * 16 + l) as u8) => i += 3,
                _ => return false,
            }
        } else {
            i += 1;
        }
    }
    true
}

/// A number field value as stored: the Go engine's `canonicalNumber`.
/// `Ok(None)` clears the field.
pub fn canonical_number(val: &Value) -> Result<Option<f64>, &'static str> {
    let f = match val {
        Value::Null => return Ok(None),
        Value::Number(n) => n.as_f64().ok_or("must be a number")?,
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return Ok(None);
            }
            if t.len() > 32 || !is_decimal(t) {
                return Err("must be a decimal number");
            }
            t.parse::<f64>().map_err(|_| "must be a decimal number")?
        }
        _ => return Err("must be a number"),
    };
    if !f.is_finite() {
        return Err("must be a finite number");
    }
    Ok(Some(f))
}

/// `^[-+]?(\d+(\.\d*)?|\.\d+)([eE][-+]?\d+)?$`
fn is_decimal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let fs = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - fs;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let es = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == es {
            return false;
        }
    }
    i == b.len()
}

/// Converts a checked value of a simple field (not file, relation or cloak)
/// to what is stored. Port of the create and update paths of the Go engine.
pub fn to_sql(f: &Field, val: &Value) -> Result<Sql, ApiError> {
    Ok(match f.kind {
        FieldType::Json => Sql::Text(
            serde_json::to_string(val)
                .map_err(|_| bad(format!("Invalid JSON field content for: {}", f.name)))?,
        ),
        FieldType::Bool => match val {
            Value::Bool(b) => Sql::Integer(*b as i64),
            Value::String(s) => {
                let l = s.trim().to_ascii_lowercase();
                Sql::Integer((l == "true" || l == "1") as i64)
            }
            Value::Number(n) => Sql::Integer((n.as_f64().unwrap_or(0.0) != 0.0) as i64),
            other => json_to_sql(other),
        },
        FieldType::Number => match canonical_number(val) {
            Ok(Some(n)) => Sql::Real(n),
            Ok(None) => Sql::Null,
            Err(e) => return Err(bad(format!("Field {:?} {e}", f.name))),
        },
        FieldType::Select => match val {
            Value::Null => Sql::Text(String::new()),
            Value::String(s) if s.is_empty() => Sql::Text(String::new()),
            Value::String(s) => {
                if !f.options.iter().any(|o| o == s) {
                    return Err(bad(format!(
                        "Invalid value {:?} for select field {} (allowed: {})",
                        s,
                        f.name,
                        f.options.join(", ")
                    )));
                }
                Sql::Text(s.clone())
            }
            _ => return Err(bad(format!("Select field {} must be a string", f.name))),
        },
        FieldType::Cloak => {
            return Err(bad(format!(
                "cloak field {:?} cannot be written: encrypted fields are not supported yet",
                f.name
            )));
        }
        _ => json_to_sql(val),
    })
}

/// A JSON value bound as SQL the way the Go driver binds `interface{}`.
pub fn json_to_sql(v: &Value) -> Sql {
    match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Sql::Integer(i),
            None => Sql::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => Sql::Text(s.clone()),
        other => Sql::Text(other.to_string()),
    }
}

/// The ids a relation field value names: one id for `1:1` and `1:N`, a
/// list (or comma-separated string) for `M:N`. Errors use the Go messages.
pub fn relation_ids(f: &Field, val: &Value) -> Result<Vec<String>, ApiError> {
    let many = f.relation.as_ref().is_some_and(|r| r.is_many());
    if !many {
        return match val {
            Value::String(s) if s.is_empty() => Ok(vec![]),
            Value::String(s) => Ok(vec![s.clone()]),
            _ => Err(bad(format!(
                "Relation field {} must be a string ID",
                f.name
            ))),
        };
    }
    match val {
        Value::Array(items) => items
            .iter()
            .map(|i| {
                i.as_str().map(str::to_owned).ok_or_else(|| {
                    bad(format!(
                        "Relation field {} elements must be strings",
                        f.name
                    ))
                })
            })
            .collect(),
        Value::String(s) => Ok(s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()),
        _ => Err(bad(format!(
            "Relation field {} must be an array of string IDs",
            f.name
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(kind: FieldType) -> Field {
        Field::new("x", kind)
    }

    #[test]
    fn numbers_are_canonical() {
        assert_eq!(canonical_number(&json!("12.50")), Ok(Some(12.5)));
        assert_eq!(canonical_number(&json!(" 3 ")), Ok(Some(3.0)));
        assert_eq!(canonical_number(&json!("")), Ok(None));
        assert_eq!(
            canonical_number(&json!("0x10")),
            Err("must be a decimal number")
        );
        assert_eq!(
            canonical_number(&json!("1_000")),
            Err("must be a decimal number")
        );
        assert_eq!(
            canonical_number(&json!("NaN")),
            Err("must be a decimal number")
        );
        assert_eq!(canonical_number(&json!(true)), Err("must be a number"));
        assert_eq!(number(42.0), json!(42));
        assert_eq!(number(0.5), json!(0.5));
    }

    #[test]
    fn parses_floats_like_go() {
        assert_eq!(go_parse_float("0x1p4"), Some(16.0));
        assert_eq!(go_parse_float("1_000.5"), Some(1000.5));
        assert_eq!(go_parse_float("-Inf"), Some(f64::NEG_INFINITY));
        assert!(go_parse_float("NaN").unwrap().is_nan());
        assert_eq!(go_parse_float("1e3"), Some(1000.0));
        assert_eq!(go_parse_float(".5"), Some(0.5));
        assert_eq!(go_parse_float("12abc"), None);
        assert_eq!(go_parse_float("_1"), None);
        assert_eq!(go_parse_float(" 1"), None);
        assert_eq!(go_parse_float("0x10"), None);
    }

    #[test]
    fn floats_print_like_go_v() {
        for (f, s) in [
            (1.0, "1"),
            (123456.0, "123456"),
            (1e6, "1e+06"),
            (1234567.0, "1.234567e+06"),
            (0.5, "0.5"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-2.5e-7, "-2.5e-07"),
            (1e21, "1e+21"),
        ] {
            assert_eq!(go_float(f), s, "{f}");
        }
        assert!(go_parse_float("0x1p99999999999999999999").is_none());
        assert_eq!(go_parse_float("0x1p99999999"), None);
        assert_eq!(go_parse_float("1e999"), None);
        assert_eq!(go_parse_float("0x1p-99999999"), Some(0.0));
    }

    #[test]
    fn urls_like_go() {
        for ok in [
            "https://moul.dev",
            "http://a.com:8080/x y?q= z",
            "http://user:pa%20ss@a.com/",
            "http://[::1]:80/",
            "https://a.com/%41",
            "http://a.com:/",
            "https://bücher.de/",
            "https://例え.jp/path",
            "http://%C3%A9.com/",
            "http://a%25b.com/",
            "http://:80/",
            "http://[::1%25eth0]/",
        ] {
            assert!(is_http_url(ok), "{ok}");
        }
        for bad in [
            "ftp://a.com",
            "http://",
            "http://a b.com/",
            "http://a.com:8x/",
            "http://[::1/",
            "http://a.com/%zz",
            "http://a.com/\u{7}",
            "mailto:a@b.c",
            "http://%41.com/",
            "http://[foo]/",
            "http://[1.2.3.4]/",
            "http://a%zz@a.com/",
        ] {
            assert!(!is_http_url(bad), "{bad}");
        }
    }

    #[test]
    fn constraints_match_go_messages() {
        let mut f = field(FieldType::Number);
        f.min = Some(1.0);
        assert_eq!(
            check_constraints(&f, Some(&json!(0))).unwrap_err().message,
            "Field \"x\" must be at least 1"
        );
        let mut f = field(FieldType::Text);
        f.required = true;
        assert_eq!(
            check_constraints(&f, Some(&json!("  ")))
                .unwrap_err()
                .message,
            "Field \"x\" is required and cannot be empty"
        );
        assert!(check_constraints(&field(FieldType::Url), Some(&json!("ftp://x"))).is_err());
        assert!(
            check_constraints(&field(FieldType::Url), Some(&json!("https://moul.dev/a?b"))).is_ok()
        );
        assert!(check_constraints(&field(FieldType::Date), Some(&json!("2026-13-01"))).is_err());
        assert!(check_constraints(&field(FieldType::Bool), Some(&json!("yes"))).is_err());
    }

    #[test]
    fn normalizes_rows() {
        let mut c = Collection::new("t", CollectionKind::Base);
        c.fields = vec![
            field(FieldType::Number),
            Field::new("b", FieldType::Bool),
            Field::new("j", FieldType::Json),
        ];
        let mut rec: Record =
            serde_json::from_value(json!({"x": 2, "b": 1, "j": "{\"a\":1}"})).unwrap();
        normalize(&c, &mut rec);
        assert_eq!(
            Value::Object(rec),
            json!({"x": 2, "b": true, "j": {"a": 1}})
        );
    }
}
