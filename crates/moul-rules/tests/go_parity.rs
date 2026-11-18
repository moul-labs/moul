//! Replays every rule call recorded from the Go engine (`fixtures/go/rules`)
//! and checks this crate answers the same: the same decision, the same SQL
//! and parameters, the same `ORDER BY` terms, the same error messages.

mod common;

use std::collections::BTreeMap;

use common::{GoDb, NOW_MS};
use hmac::{Hmac, Mac};
use moul_fixtures::{Fixtures, RuleCase, RuleKind};
use moul_rules::{EvalContext, FilterOptions, Map, SortOptions, evaluate, filter_sql, sort_sql};
use moul_store::schema::Collection;
use serde_json::{Value, json};
use sha2::Sha256;

/// Cases whose error text comes from expr-lang internals that are not
/// reproduced: only that an error occurred is compared. Empty: every
/// recorded message is reproduced.
const ERROR_TEXT_NOT_COMPARED: &[(&str, &str)] = &[];

/// Cases that are known to differ, with the reason. Empty.
const KNOWN_DIFFERENCES: &[(&str, &str)] = &[];

#[test]
fn evaluate_matches_go() {
    let fx = Fixtures::default();
    let cases = fx.rules(RuleKind::Evaluate).expect("read evaluate.jsonl");
    let mut failures = Vec::new();
    for case in &cases {
        let input = &case.input;
        let db = GoDb::new();
        if let Some(id) = input.get("db").and_then(Value::as_str) {
            fx.db(id)
                .expect("read database state")
                .load_into(&db.conn)
                .expect("load database state");
        }
        let empty = Map::new();
        let ctx = EvalContext {
            auth: input["auth"].as_object(),
            record: input["record"].as_object().unwrap_or(&empty),
            request: input["context"].as_object(),
            // The Go engine always had a database here; only rules that
            // query it had their state recorded.
            db: Some(&db),
            now_ms: NOW_MS,
        };
        let got = match evaluate(input["rule"].as_str().unwrap_or_default(), &ctx) {
            Ok(allowed) => json!({"allowed": allowed, "error": null}),
            Err(e) => json!({"allowed": false, "error": e.message()}),
        };
        compare(case, got, &mut failures);
    }
    report("evaluate", cases.len(), failures);
}

#[test]
fn filter_sql_matches_go() {
    let fx = Fixtures::default();
    let cases = fx
        .rules(RuleKind::FilterSql)
        .expect("read filter_sql.jsonl");
    let mut failures = Vec::new();
    for case in &cases {
        let input = &case.input;
        let collection: Collection =
            serde_json::from_value(input["moul"].clone()).expect("collection");
        let loaded = loaded(input);
        let load = |name: &str| loaded.get(name).cloned().flatten();
        let key = cloak_key(case);
        let hash = |text: &str| key.map(|k| blind_index(k, text)).unwrap_or_default();
        let opts = FilterOptions {
            param_prefix: input["param_prefix"].as_str().unwrap_or_default(),
            untrusted: input["untrusted"].as_bool().unwrap_or_default(),
            load: input["has_loader"]
                .as_bool()
                .unwrap_or_default()
                .then_some(&load as _),
            now_ms: NOW_MS,
            cloak_hash: Some(&hash),
        };
        let got = filter_sql(
            input["filter"].as_str().unwrap_or_default(),
            &collection,
            input["auth"].as_object(),
            input["context"].as_object(),
            &opts,
        );
        let got = match got {
            Ok(None) => json!({"where": "", "params": null, "error": null}),
            Ok(Some(f)) => {
                let mut params = Map::new();
                for (k, v) in f.params {
                    assert!(
                        params.insert(k, v).is_none(),
                        "{}: duplicate parameter name",
                        case.id
                    );
                }
                json!({"where": f.sql, "params": params, "error": null})
            }
            Err(e) => json!({"where": "", "params": null, "error": e.message()}),
        };
        let mut want = case.output.clone();
        // Go writes dbx placeholders, `{:p1}`; this crate writes `:p1`.
        if let Some(w) = want["where"].as_str() {
            want["where"] = Value::String(dbx_to_named(w));
        }
        compare(
            &RuleCase {
                output: want,
                ..clone_case(case)
            },
            got,
            &mut failures,
        );
    }
    report("filter_sql", cases.len(), failures);
}

#[test]
fn sort_sql_matches_go() {
    let fx = Fixtures::default();
    let cases = fx.rules(RuleKind::SortSql).expect("read sort_sql.jsonl");
    let mut failures = Vec::new();
    for case in &cases {
        let input = &case.input;
        let collection: Collection =
            serde_json::from_value(input["moul"].clone()).expect("collection");
        let loaded = loaded(input);
        let load = |name: &str| loaded.get(name).cloned().flatten();
        let opts = SortOptions {
            untrusted: input["untrusted"].as_bool().unwrap_or_default(),
            load: input["has_loader"]
                .as_bool()
                .unwrap_or_default()
                .then_some(&load as _),
        };
        let got = match sort_sql(
            input["sort"].as_str().unwrap_or_default(),
            &collection,
            &opts,
        ) {
            Ok(terms) if terms.is_empty() => json!({"order_by": null, "error": null}),
            Ok(terms) => json!({"order_by": terms, "error": null}),
            Err(e) => json!({"order_by": null, "error": e.message()}),
        };
        compare(case, got, &mut failures);
    }
    report("sort_sql", cases.len(), failures);
}

/// The relation targets a call looked up: a collection, or `None` where
/// the Go loader failed.
fn loaded(input: &Value) -> BTreeMap<String, Option<Collection>> {
    input["loaded"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, v)| {
            (
                name.clone(),
                v.get("moul")
                    .map(|m| serde_json::from_value(m.clone()).expect("loaded")),
            )
        })
        .collect()
}

fn clone_case(case: &RuleCase) -> RuleCase {
    RuleCase {
        id: case.id.clone(),
        sources: case.sources.clone(),
        input: case.input.clone(),
        output: case.output.clone(),
    }
}

/// Compares one case. Numbers compare by value, as Go's JSON does not keep
/// integer and float apart.
fn compare(case: &RuleCase, mut got: Value, failures: &mut Vec<String>) {
    if KNOWN_DIFFERENCES.iter().any(|(id, _)| *id == case.id) {
        return;
    }
    let mut want = case.output.clone();
    if ERROR_TEXT_NOT_COMPARED.iter().any(|(id, _)| *id == case.id) {
        for v in [&mut want, &mut got] {
            if v["error"].is_string() {
                v["error"] = json!("<error>");
            }
        }
    }
    if !json_eq(&want, &got) {
        failures.push(format!(
            "case {} ({})\n  input: {}\n  want:  {}\n  got:   {}",
            case.id,
            case.sources.join(", "),
            short(&case.input),
            want,
            got
        ));
    }
}

fn report(kind: &str, total: usize, failures: Vec<String>) {
    println!(
        "{kind}: {} of {total} cases match the Go engine",
        total - failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} {kind} cases differ:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// The input without the bulky collection definitions.
fn short(input: &Value) -> String {
    let mut v = input.clone();
    if let Some(o) = v.as_object_mut() {
        if let Some(name) = o.get("moul").and_then(|m| m.get("name")).cloned() {
            o.insert("moul".into(), name);
        }
        if let Some(l) = o.get("loaded").and_then(Value::as_object) {
            let names: Vec<Value> = l.keys().map(|k| json!(k)).collect();
            o.insert("loaded".into(), Value::Array(names));
        }
    }
    v.to_string()
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

/// `{:name}` placeholders become `:name`.
fn dbx_to_named(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(i) = rest.find("{:") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find('}') {
            Some(j) => {
                out.push(':');
                out.push_str(&after[..j]);
                rest = &after[j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The cloak master key the recording Go test set, by test name.
fn cloak_key(case: &RuleCase) -> Option<&'static str> {
    let source = |name: &str| case.sources.iter().any(|s| s.ends_with(name));
    if source(".TestBuildFilterSQL_Cloak") {
        Some("test-secret-key-1234567890-cloak")
    } else if source(".TestCloak_EndToEnd") {
        Some("test-encryption-key-for-records-testing-123456")
    } else {
        None
    }
}

/// The Go engine's cloak blind index: HMAC-SHA256 of the text under a key
/// derived from the master key with HKDF-SHA256, as lowercase hex.
fn blind_index(master_key: &str, text: &str) -> String {
    fn hmac(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
        let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
        for p in parts {
            m.update(p);
        }
        m.finalize().into_bytes().to_vec()
    }
    let prk = hmac(b"moul-cloak-kdf-salt-v1", &[master_key.trim().as_bytes()]);
    let key = hmac(&prk, &[b"moul-cloak-blindindex-v1", &[1]]);
    hmac(&key, &[text.as_bytes()])
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
