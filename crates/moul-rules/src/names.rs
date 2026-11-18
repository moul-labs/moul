//! The names a record rule reads, checked against its collection before the
//! rule is saved (`moul check`). The engine itself does not check them.

use moul_store::schema::{Collection, FieldType};

use crate::go;
use crate::sql::{LoadCollection, is_valid_field};
use crate::tokenize::{Kind, tokenize};
use crate::translate::is_date_macro;
use crate::{Error, check};

/// Words of the expression language a rule becomes, and `null`: none of
/// them reads a column.
const WORDS: &[&str] = &[
    "true",
    "false",
    "nil",
    "null",
    "not",
    "in",
    "matches",
    "contains",
    "startsWith",
    "endsWith",
    "let",
    "if",
    "else",
];

/// Checks a record rule (view, create, update, delete or subscribe) against
/// its collection and returns every problem; an empty list means the rule
/// reads only what the collection has.
///
/// These rules are evaluated against one record (see
/// [`evaluate`](crate::evaluate)). A bare name reads a column of that
/// record, so a misspelt one reads as null instead of failing. A relation
/// path such as `team.owner` fails to compile once the relation holds an id:
/// only a list rule, compiled by [`filter_sql`](crate::filter_sql), follows
/// relations. An `@collection.<table>.<column>` condition is checked
/// against that collection when `load` finds it; without `load` it is not
/// checked. The caller (`@request.auth.*`), the rest of the request
/// (`@request.body.*`, `@request.method`, ...), date macros, functions and
/// literals are not the record's and are left alone. A rule with a syntax
/// error gives that error alone, as [`check`] reports it.
pub fn record_rule_names(
    rule: &str,
    collection: &Collection,
    load: Option<LoadCollection<'_>>,
) -> Vec<Error> {
    if let Err(e) = check(rule) {
        return vec![e];
    }
    // `check` tokenized the rule already.
    let Ok(tokens) = tokenize(rule) else {
        return Vec::new();
    };
    let mut out: Vec<Error> = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let called = tokens
            .get(i + 1)
            .is_some_and(|n| n.is(Kind::Paren) && n.value == "(");
        if !t.is(Kind::Identifier) || called || WORDS.contains(&t.value.as_str()) {
            continue;
        }
        if let Err(e) = name(&t.value, collection, load)
            && !out.contains(&e)
        {
            out.push(e);
        }
    }
    out
}

/// The collections a rule names with `@collection.<name>` (or
/// `@collection.<name>:<alias>`), each once, in the order they first
/// appear. Those conditions read their tables directly, so whatever runs
/// the rule depends on these collections too. A rule that cannot be read
/// names none: it fails before it reads anything.
pub fn collections_named(rule: &str) -> Vec<String> {
    if !rule.contains("@collection.") {
        return Vec::new();
    }
    let Ok(tokens) = tokenize(rule) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for t in tokens.iter().filter(|t| t.is(Kind::Identifier)) {
        let Some(rest) = t.value.strip_prefix("@collection.") else {
            continue;
        };
        let key = rest.split_once('.').map_or(rest, |(key, _)| key);
        let table = key.split_once(':').map_or(key, |(table, _)| table);
        if !table.is_empty() && !out.iter().any(|n| n == table) {
            out.push(table.to_owned());
        }
    }
    out
}

/// Whether `path` reads the caller, the request or the clock rather than
/// the record.
fn not_the_record(path: &str) -> bool {
    let first = path.split('.').next().unwrap_or_default();
    path.starts_with('@')
        || matches!(first, "auth" | "request" | "_record")
        || first.strip_prefix('_').is_some_and(is_date_macro)
}

fn name(value: &str, c: &Collection, load: Option<LoadCollection<'_>>) -> Result<(), Error> {
    if let Some(rest) = value.strip_prefix("@collection.") {
        return collection_column(value, rest, load);
    }
    let (path, modifier) = match value.split_once(':') {
        Some((path, modifier)) => (path, Some(modifier)),
        None => (value, None),
    };
    match modifier {
        // `x.key:changed` compares the record's `key` with the request
        // body's, whatever `x` is.
        Some("changed") => match path.rsplit_once('.') {
            Some((_, key)) if !is_valid_field(key, c) => Err(Error::new(format!(
                "invalid field {} in {}",
                go::quote(key),
                go::quote(value)
            ))),
            _ => Ok(()),
        },
        // `x.key:isset` asks whether `x` has `key`.
        Some("isset") => match path.rsplit_once('.') {
            Some((parent, _)) => column_path(parent, c),
            None => Ok(()),
        },
        None | Some("lower" | "length" | "each") => column_path(path, c),
        // Any other modifier fails to translate, which `check` reports.
        Some(_) => Ok(()),
    }
}

/// A column of the record, or a path into a JSON (or other) column.
fn column_path(path: &str, c: &Collection) -> Result<(), Error> {
    if not_the_record(path) {
        return Ok(());
    }
    let (first, rest) = match path.split_once('.') {
        Some((first, rest)) => (first, Some(rest)),
        None => (path, None),
    };
    if !go::is_ident(first) || !is_valid_field(first, c) {
        return Err(Error::new(format!("invalid field {}", go::quote(first))));
    }
    let relation = c
        .fields
        .iter()
        .any(|f| f.name == first && f.kind == FieldType::Relation);
    if relation && rest.is_some() {
        return Err(Error::new(format!(
            "{} follows the relation {}, which only a list rule can do",
            go::quote(path),
            go::quote(first)
        )));
    }
    Ok(())
}

/// `@collection.<table>.<column>` or `@collection.<table>:<alias>.<column>`.
fn collection_column(
    value: &str,
    rest: &str,
    load: Option<LoadCollection<'_>>,
) -> Result<(), Error> {
    let (Some(load), Some((key, column))) = (load, rest.split_once('.')) else {
        return Ok(());
    };
    let table = key.split_once(':').map_or(key, |(table, _)| table);
    let Some(target) = load(table) else {
        return Err(Error::new(format!(
            "unknown collection {} in {}",
            go::quote(table),
            go::quote(value)
        )));
    };
    if go::is_ident(column) && !is_valid_field(column, &target) {
        return Err(Error::new(format!(
            "invalid field {} in {}",
            go::quote(column),
            go::quote(value)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tasks() -> Collection {
        serde_json::from_value(json!({
            "name": "tasks",
            "fields": [
                {"name": "title", "type": "text"},
                {"name": "owner", "type": "text"},
                {"name": "meta", "type": "json"},
                {"name": "role", "type": "text"},
                {"name": "team", "type": "relation",
                 "relationConfig": {"targetMoul": "teams", "cardinality": "1:N"}},
            ],
        }))
        .unwrap()
    }

    fn members() -> Collection {
        serde_json::from_value(json!({
            "name": "members",
            "fields": [{"name": "user", "type": "text"}],
        }))
        .unwrap()
    }

    fn problems(rule: &str) -> Vec<String> {
        let load = |n: &str| (n == "members").then(members);
        record_rule_names(rule, &tasks(), Some(&load))
            .into_iter()
            .map(|e| e.message().to_owned())
            .collect()
    }

    #[test]
    fn accepts_what_the_record_and_request_hold() {
        for rule in [
            "",
            "owner = @request.auth.id",
            "id = auth.id && title != ''",
            "@request.body.owner = @request.auth.id && @request.method = 'POST'",
            "@request.body.role:isset = false && @request.body.role:changed = false",
            "title:lower = 'x' && title:length > 2 && meta.a.b = 1 && meta.tags:each = 'x'",
            "createdAt < @now && updatedAt > @todayStart",
            "lower(title) = 'x' && geoDistance(1, 2, 3, 4) < 5",
            "@collection.members.user = @request.auth.id && @collection.members:m.user = owner",
            "team = null || team != ''",
            "true",
        ] {
            assert_eq!(problems(rule), Vec::<String>::new(), "{rule}");
        }
    }

    #[test]
    fn finds_unknown_columns_and_relation_paths() {
        assert_eq!(
            problems("ownr = @request.auth.id || ownr = '' || titel != ''"),
            [r#"invalid field "ownr""#, r#"invalid field "titel""#]
        );
        assert_eq!(
            problems("team.owner = @request.auth.id"),
            [r#""team.owner" follows the relation "team", which only a list rule can do"#]
        );
        assert_eq!(
            problems("@request.body.rol:changed = false && nme:lower = 'x' && x.y:isset = true"),
            [
                r#"invalid field "rol" in "@request.body.rol:changed""#,
                r#"invalid field "nme""#,
                r#"invalid field "x""#,
            ]
        );
        assert_eq!(
            problems("@collection.teams.user = 'x' && @collection.members.usr = 'x'"),
            [
                r#"unknown collection "teams" in "@collection.teams.user""#,
                r#"invalid field "usr" in "@collection.members.usr""#,
            ]
        );
        assert_eq!(
            problems("owner = 'x"),
            ["failed to translate rule: unterminated string literal starting at position 8"]
        );
    }

    #[test]
    fn names_the_collections_a_rule_reads() {
        assert_eq!(
            collections_named(
                "@collection.members.user = @request.auth.id && \
                 (@collection.members:m.role = 'admin' || @collection.teams.owner = owner)"
            ),
            ["members", "teams"]
        );
        assert!(collections_named("owner = '@collection.members.user'").is_empty());
        assert!(collections_named("owner = @request.auth.id").is_empty());
        assert!(collections_named("@collection.members.user = 'x").is_empty());
    }

    #[test]
    fn leaves_collection_conditions_alone_without_load() {
        let found = record_rule_names("@collection.teams.x = 'y'", &tasks(), None);
        assert!(found.is_empty(), "{found:?}");
    }
}
