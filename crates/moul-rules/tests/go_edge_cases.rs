//! Rules the Go test suite never tried, answered by the Go engine itself.
//!
//! The files in `tests/data` were made by running the Go engine's
//! `rules.EvaluateRule` (internal/rules at the commit in
//! `fixtures/go/MANIFEST.json`, expr-lang v1.17.8) on each input and
//! recording the result:
//!
//! - `go_edge_cases.jsonl`, without a database, probes what expr-lang
//!   decides: type errors found while checking, errors met while running,
//!   constant folding, odd spacing that glues tokens together, and the
//!   messages and positions of each; and text that Go's `ParseFloat` reads
//!   as a number or not (out of range, `_`, a signed `NaN`).
//! - `go_edge_cases_db.jsonl` has a database (its first line holds the SQL
//!   that builds it, on modernc.org/sqlite) and probes `get(`, `exists(`,
//!   `strftime(` and `@collection`: how values are bound and read back.
//! - `go_sql_edge_cases.jsonl` holds calls of `BuildFilterSQLWithOptions`
//!   and `BuildSortSQLWithOptions` over three collections, trusted and not.
//!   Go's cloak hash was replaced by `hash(<text>)` for the recording.
//!
//! No case reads the clock in a way that changes its result.

mod common;

use std::collections::BTreeMap;

use common::GoDb;
use moul_rules::{
    EvalContext, FilterOptions, Lookup, Map, SortOptions, evaluate, filter_sql, sort_sql,
};
use moul_store::schema::Collection;
use serde_json::{Value, json};

#[test]
fn evaluate_matches_go_on_edge_cases() {
    replay(
        "without a database",
        include_str!("data/go_edge_cases.jsonl"),
    );
}

#[test]
fn evaluate_matches_go_on_database_edge_cases() {
    replay(
        "with a database",
        include_str!("data/go_edge_cases_db.jsonl"),
    );
}

fn replay(what: &str, text: &str) {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty()).peekable();
    let setup: Option<Vec<String>> = lines.next_if(|l| l.starts_with("{\"setup\"")).map(|l| {
        let v: Value = serde_json::from_str(l).expect("setup");
        let statements = v["setup"].as_array().expect("statements");
        statements
            .iter()
            .map(|s| s.as_str().expect("statement").to_owned())
            .collect()
    });
    let mut failures = Vec::new();
    let mut total = 0;
    for line in lines {
        total += 1;
        let case: Value = serde_json::from_str(line).expect("edge case");
        let input = &case["input"];
        let db = setup.as_ref().map(|statements| {
            let db = GoDb::new();
            for s in statements {
                db.conn
                    .execute_batch(s)
                    .unwrap_or_else(|e| panic!("{s}: {e}"));
            }
            db
        });
        let empty = Map::new();
        let ctx = EvalContext {
            auth: input["auth"].as_object(),
            record: input["record"].as_object().unwrap_or(&empty),
            request: input["context"].as_object(),
            db: db.as_ref().map(|d| d as &dyn Lookup),
            now_ms: common::NOW_MS,
        };
        let rule = input["rule"].as_str().unwrap_or_default();
        let got = match evaluate(rule, &ctx) {
            Ok(allowed) => json!({"allowed": allowed, "error": null}),
            Err(e) => json!({"allowed": false, "error": e.message()}),
        };
        if got != case["output"] {
            failures.push(format!(
                "rule {rule:?}\n  want: {}\n  got:  {got}",
                case["output"]
            ));
        }
    }
    println!(
        "edge cases {what}: {} of {total} match the Go engine",
        total - failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} edge cases differ:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn sql_matches_go_on_edge_cases() {
    let text = include_str!("data/go_sql_edge_cases.jsonl");
    let mut failures = Vec::new();
    let mut total = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        total += 1;
        let case: Value = serde_json::from_str(line).expect("edge case");
        let input = &case["input"];
        let collection: Collection =
            serde_json::from_value(input["moul"].clone()).expect("collection");
        let loader: Option<BTreeMap<String, Collection>> = input["loader"].as_object().map(|m| {
            m.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        serde_json::from_value(v.clone()).expect("loaded"),
                    )
                })
                .collect()
        });
        let load = |name: &str| loader.as_ref().and_then(|l| l.get(name).cloned());
        let load = loader
            .is_some()
            .then_some(&load as &dyn Fn(&str) -> Option<Collection>);
        let text = input["filter"].as_str().unwrap_or_default();
        let untrusted = input["untrusted"].as_bool().unwrap_or_default();
        let got = if input["kind"] == "sort" {
            match sort_sql(text, &collection, &SortOptions { untrusted, load }) {
                Ok(terms) if terms.is_empty() => json!({"order_by": null, "error": null}),
                Ok(terms) => json!({"order_by": terms, "error": null}),
                Err(e) => json!({"order_by": null, "error": e.message()}),
            }
        } else {
            let hash = |text: &str| format!("hash({text})");
            let opts = FilterOptions {
                param_prefix: input["param_prefix"].as_str().unwrap_or_default(),
                untrusted,
                load,
                now_ms: common::NOW_MS,
                cloak_hash: Some(&hash),
            };
            match filter_sql(
                text,
                &collection,
                input["auth"].as_object(),
                input["context"].as_object(),
                &opts,
            ) {
                Ok(None) => json!({"where": "", "params": null, "error": null}),
                Ok(Some(f)) => {
                    let sql = f.sql;
                    let params: Map = f.params.into_iter().collect();
                    json!({"where": sql, "params": params, "error": null})
                }
                Err(e) => json!({"where": "", "params": null, "error": e.message()}),
            }
        };
        let mut want = case["output"].clone();
        if let Some(w) = want["where"].as_str() {
            want["where"] = Value::String(w.replace("{:", ":").replace('}', ""));
        }
        if !json_eq(&want, &got) {
            failures.push(format!(
                "{} {text:?} (untrusted {untrusted})\n  want: {want}\n  got:  {got}",
                input["kind"]
            ));
        }
    }
    println!(
        "SQL edge cases: {} of {total} match the Go engine",
        total - failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} SQL edge cases differ:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Numbers compare by value: Go's JSON does not keep integer and float apart.
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
