//! The rules of a collection checked against it before they are saved: by
//! the engine on every definition change, and by `moul check`.

use moul_store::schema::{Collection, FieldType};

use crate::go;
use crate::sql::{FilterOptions, LoadCollection, filter_sql, is_valid_field};
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

/// The rules of `collection`, each with the key the API and a schema file
/// give it, in this order: `listRule`, `viewRule`, `createRule`,
/// `updateRule`, `deleteRule`, `subscribeRule`.
pub fn rules_by_key(collection: &Collection) -> [(&'static str, &str); 6] {
    let r = &collection.rules;
    [
        ("listRule", &r.list_rule),
        ("viewRule", &r.view_rule),
        ("createRule", &r.create_rule),
        ("updateRule", &r.update_rule),
        ("deleteRule", &r.delete_rule),
        ("subscribeRule", &r.subscribe_rule),
    ]
}

/// Checks every rule of `collection` as it will run, and returns each
/// problem with the key of its rule (see [`rules_by_key`]); an empty list
/// means every rule compiles.
///
/// The list rule is compiled to SQL as a list compiles it ([`filter_sql`]),
/// relation paths followed through `load`. Its `@collection.<name>.<field>`
/// conditions become SQL as written, so `load` must find that collection
/// and field. It cannot call a function (`exists(`, `get(`): only the other
/// rules can, which are evaluated against one record and checked with
/// [`record_rule_names`]. A blank list rule and an empty record rule let
/// everyone through. The caller and the request only supply values, so
/// neither is needed.
pub fn rule_errors(
    collection: &Collection,
    load: Option<LoadCollection<'_>>,
) -> Vec<(&'static str, Error)> {
    let mut out = Vec::new();
    for (key, rule) in rules_by_key(collection) {
        if key == "listRule" {
            if rule.trim().is_empty() {
                continue;
            }
            out.extend(
                list_rule_errors(rule, collection, load)
                    .into_iter()
                    .map(|e| (key, e)),
            );
        } else if !rule.is_empty() {
            out.extend(
                record_rule_names(rule, collection, load)
                    .into_iter()
                    .map(|e| (key, e)),
            );
        }
    }
    out
}

/// The problems of a list rule that is not blank; see [`rule_errors`].
fn list_rule_errors(
    rule: &str,
    collection: &Collection,
    load: Option<LoadCollection<'_>>,
) -> Vec<Error> {
    let tokens = tokenize(rule).unwrap_or_default();
    // Compiling these fails as an "invalid field": say why.
    if let Some(w) = tokens
        .windows(2)
        .find(|w| w[0].is(Kind::Identifier) && w[1].is(Kind::Paren) && w[1].value == "(")
    {
        return vec![Error::new(format!(
            "a list rule cannot call {}(): use @collection.<name>.<field> or a relation path",
            w[0].value
        ))];
    }
    if let Some(t) = tokens
        .iter()
        .find(|t| t.is(Kind::Identifier) && t.value.starts_with("@request.body."))
    {
        return vec![Error::new(format!(
            "a list rule cannot read {}: a list has no body",
            t.value
        ))];
    }
    let opts = FilterOptions {
        param_prefix: "rule_",
        untrusted: false,
        load,
        now_ms: 0,
        cloak_hash: None,
    };
    if let Err(e) = filter_sql(rule, collection, None, None, &opts) {
        return vec![e];
    }
    let mut out: Vec<Error> = Vec::new();
    for t in tokens.iter().filter(|t| t.is(Kind::Identifier)) {
        if let Some(rest) = t.value.strip_prefix("@collection.")
            && let Err(e) = collection_column(&t.value, rest, load)
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

    fn teams() -> Collection {
        serde_json::from_value(json!({
            "name": "teams",
            "fields": [{"name": "owner", "type": "text"}],
        }))
        .unwrap()
    }

    /// The problems of `tasks` with `rules`, as `key: message`.
    fn rule_problems(rules: serde_json::Value) -> Vec<String> {
        let mut c = tasks();
        c.rules = serde_json::from_value(rules).unwrap();
        let load = |n: &str| match n {
            "members" => Some(members()),
            "teams" => Some(teams()),
            _ => None,
        };
        rule_errors(&c, Some(&load))
            .into_iter()
            .map(|(key, e)| format!("{key}: {}", e.message()))
            .collect()
    }

    #[test]
    fn every_rule_is_checked_as_it_runs() {
        let fine = json!({
            "listRule": "team.owner = @request.auth.id || @collection.members.user = @request.auth.id",
            "viewRule": "exists('members', 'user = ?', @request.auth.id) && get('members', 'user = ?', owner) != nil",
            "createRule": "@request.body.owner = @request.auth.id",
            "updateRule": "true",
            "deleteRule": "",
            "subscribeRule": "",
        });
        assert_eq!(rule_problems(fine), Vec::<String>::new());
        assert_eq!(
            rule_problems(json!({"listRule": "  "})),
            Vec::<String>::new()
        );

        // There is no `!`, and a condition is not a value.
        assert_eq!(
            rule_problems(json!({
                "listRule": "!(title = 'x')",
                "viewRule": "!(title = 'x')",
            })),
            [
                "listRule: failed to tokenize filter: unexpected character '!' at position 0",
                "viewRule: failed to translate rule: unexpected character '!' at position 0",
            ]
        );
        assert_eq!(
            rule_problems(json!({"listRule": "(title = 'x') = false"})),
            [r#"listRule: unexpected token "=" at position 5"#]
        );
        // Fine in a record rule, not in a list rule: a list has no body.
        assert_eq!(
            rule_problems(json!({
                "listRule": "@request.body.title = 'x'",
                "createRule": "@request.body.title = 'x'",
            })),
            ["listRule: a list rule cannot read @request.body.title: a list has no body"]
        );
        assert_eq!(
            rule_problems(json!({"listRule": "exists('members', 'user = ?', @request.auth.id)"})),
            [
                "listRule: a list rule cannot call exists(): use @collection.<name>.<field> or a relation path"
            ]
        );
        // `@collection` reads the table as written: it must be a collection.
        assert_eq!(
            rule_problems(json!({
                "listRule": "@collection.moul_sync.key != '' && @collection.members.usr = 'x'",
            })),
            [
                r#"listRule: unknown collection "moul_sync" in "@collection.moul_sync.key""#,
                r#"listRule: invalid field "usr" in "@collection.members.usr""#,
            ]
        );
        assert_eq!(
            rule_problems(json!({
                "listRule": "team.ownr = ''",
                "deleteRule": "ownr = '' || titel = ''",
                "subscribeRule": "owner = 'x",
            })),
            [
                r#"listRule: invalid field "ownr" in path "team.ownr""#,
                r#"deleteRule: invalid field "ownr""#,
                r#"deleteRule: invalid field "titel""#,
                "subscribeRule: failed to translate rule: unterminated string literal starting at position 8",
            ]
        );
    }

    #[test]
    fn leaves_collection_conditions_alone_without_load() {
        let found = record_rule_names("@collection.teams.x = 'y'", &tasks(), None);
        assert!(found.is_empty(), "{found:?}");
    }
}
