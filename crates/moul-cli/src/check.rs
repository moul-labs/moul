//! `moul check`: an app definition checked before it is applied.
//!
//! A schema file goes through the stages its application would, each only
//! once the one before it found no error: the JSON and its format, each
//! definition, then applying the whole file to an empty in-memory engine.
//! The collections the engine then holds are checked as a database's
//! are: relation targets, every rule against its collection, and rules
//! that let anyone write. Screens join these checks with the manifest.

use moul_core::definition::{SCHEMA_FORMAT, read_definition};
use moul_core::{Config, Engine, MemoryBlobs, Request, SeededHost};
use moul_rules::FilterOptions;
use moul_store::Store;
use moul_store::schema::{Collection, CollectionKind, FieldType};
use serde::Serialize;
use serde_json::{Value, json};

/// What `moul check` reads.
pub(crate) enum Input {
    /// The bytes of a schema file.
    Schema(Vec<u8>),
    /// The collections a database holds.
    Database(Vec<Collection>),
}

/// One problem, and where it is.
#[derive(Serialize)]
pub(crate) struct Finding {
    location: String,
    message: String,
}

/// Everything `moul check` found in one file.
pub(crate) struct Report {
    file: String,
    collections: usize,
    errors: Vec<Finding>,
    warnings: Vec<Finding>,
}

impl Report {
    fn error(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.errors.push(Finding {
            location: location.into(),
            message: message.into(),
        });
    }

    fn warn(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(Finding {
            location: location.into(),
            message: message.into(),
        });
    }

    /// Whether the definition passes: no error and, when `strict`, no
    /// warning either.
    pub(crate) fn passes(&self, strict: bool) -> bool {
        self.errors.is_empty() && !(strict && !self.warnings.is_empty())
    }

    /// The report as `--json` prints it, keys in this order. Agents parse
    /// it: keep its shape.
    pub(crate) fn to_json(&self, strict: bool) -> String {
        #[derive(Serialize)]
        struct Json<'a> {
            ok: bool,
            file: &'a str,
            collections: usize,
            errors: &'a [Finding],
            warnings: &'a [Finding],
        }
        let mut text = serde_json::to_string_pretty(&Json {
            ok: self.passes(strict),
            file: &self.file,
            collections: self.collections,
            errors: &self.errors,
            warnings: &self.warnings,
        })
        .expect("json serialises");
        text.push('\n');
        text
    }

    /// The report as people read it: a line per finding and a summary.
    pub(crate) fn to_text(&self, strict: bool) -> String {
        let mut out = String::new();
        for f in &self.errors {
            out += &format!("error: {}: {}\n", f.location, f.message);
        }
        for f in &self.warnings {
            out += &format!("warning: {}: {}\n", f.location, f.message);
        }
        let count = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        if self.passes(strict) {
            out += &format!(
                "{} is valid: {}",
                self.file,
                count(self.collections, "collection")
            );
            if !self.warnings.is_empty() {
                out += &format!(", {}", count(self.warnings.len(), "warning"));
            }
        } else {
            let mut parts = Vec::new();
            if !self.errors.is_empty() {
                parts.push(count(self.errors.len(), "error"));
            }
            if !self.warnings.is_empty() {
                parts.push(count(self.warnings.len(), "warning"));
            }
            out += &format!("{} in {}", parts.join(", "), self.file);
        }
        out.push('\n');
        out
    }
}

/// Checks `input`, shown as `file` in the report. An error is returned only
/// when the check itself cannot run.
pub(crate) fn check(input: Input, file: &str) -> Result<Report, String> {
    let mut r = Report {
        file: file.to_owned(),
        collections: 0,
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    match input {
        Input::Schema(bytes) => {
            if let Some(applied) = check_schema(&mut r, &bytes)? {
                check_collections(&mut r, &applied);
            }
        }
        Input::Database(mut stored) => {
            // In the order `moul schema` writes them, so locations match
            // the schema file of this database.
            stored.sort_by(|a, b| a.name.cmp(&b.name));
            r.collections = stored.len();
            let stored: Vec<(usize, Collection)> = stored.into_iter().enumerate().collect();
            // Stored collections are in use: each is checked as its next
            // change would check it, not applied afresh.
            for (i, c) in &stored {
                if let Err(e) = c.clone().validate(Some(c)) {
                    r.error(place(*i, &c.name), e.to_string());
                }
            }
            check_collections(&mut r, &stored);
        }
    }
    Ok(r)
}

/// `collections[2] (tasks)`, or `collections[2]` for a nameless one.
fn place(i: usize, name: &str) -> String {
    match name.trim() {
        "" => format!("collections[{i}]"),
        name => format!("collections[{i}] ({name})"),
    }
}

/// An in-memory engine with no collections.
fn empty_engine() -> Result<Engine, String> {
    Engine::open(
        Store::open_in_memory()
            .map_err(|e| format!("cannot start an engine to check with: {e}"))?,
        SeededHost::new(1, 0),
        MemoryBlobs::new(),
        Config::default(),
    )
    .map_err(|e| format!("cannot start an engine to check with: {e}"))
}

/// Applies a schema file as `POST /api/schema` does.
fn apply(engine: &mut Engine, file: Value) -> Result<(), String> {
    let r = engine.handle(Request::post("/api/schema", file).owner());
    match r.status {
        200 => Ok(()),
        _ => Err(match r.message() {
            "" => format!("the engine refused the schema file (HTTP {})", r.status),
            m => m.to_owned(),
        }),
    }
}

/// The file's own stages. Returns the collections an empty engine holds
/// once the file is applied, by their place in the file, or `None` when
/// an earlier stage found errors.
fn check_schema(r: &mut Report, bytes: &[u8]) -> Result<Option<Vec<(usize, Collection)>>, String> {
    let file: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            let text = e.to_string();
            let at = format!(" at line {} column {}", e.line(), e.column());
            let message = text.strip_suffix(&at).unwrap_or(&text);
            r.error(
                format!("line {}, column {}", e.line(), e.column()),
                format!("not JSON: {message}"),
            );
            return Ok(None);
        }
    };
    let Some(top) = file.as_object() else {
        r.error(
            "file",
            format!(
                "a schema file is a JSON object: {{\"format\": {SCHEMA_FORMAT}, \"collections\": [...]}}"
            ),
        );
        return Ok(None);
    };
    // The engine reads a file without `format` as the current format.
    if let Some(format) = top.get("format") {
        match format.as_i64() {
            Some(n) if (1..=SCHEMA_FORMAT).contains(&n) => {}
            Some(n) => r.error(
                "format",
                format!(
                    "the schema file has format {n}; this engine reads format {SCHEMA_FORMAT} and older"
                ),
            ),
            None => r.error(
                "format",
                format!("format must be a whole number, such as {SCHEMA_FORMAT}"),
            ),
        }
    }
    let Some(items) = top.get("collections").and_then(Value::as_array) else {
        r.error(
            "collections",
            "the schema file needs a \"collections\" array",
        );
        return Ok(None);
    };
    r.collections = items.len();
    if !r.errors.is_empty() {
        return Ok(None);
    }

    let mut defs: Vec<(usize, Collection)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        match read_definition(item) {
            Ok(c) => defs.push((i, c)),
            Err(e) => r.error(place(i, name), e),
        }
    }
    // Applying matches collections by id, then by name, so a second one
    // with the same id or name would quietly change the first.
    for (k, (i, c)) in defs.iter().enumerate() {
        let earlier = &defs[..k];
        if let Some((j, o)) = earlier
            .iter()
            .find(|(_, o)| o.name.eq_ignore_ascii_case(&c.name))
        {
            r.error(
                place(*i, &c.name),
                format!("{} has the same name", place(*j, &o.name)),
            );
        } else if let Some((j, o)) = earlier
            .iter()
            .find(|(_, o)| !c.id.is_empty() && o.id == c.id)
        {
            r.error(
                place(*i, &c.name),
                format!("the id {:?} is also the id of {}", c.id, place(*j, &o.name)),
            );
        }
    }
    // Applying refuses a relation to a collection the file does not make;
    // say which field holds it.
    relation_targets(r, &defs);
    if !r.errors.is_empty() {
        return Ok(None);
    }

    let mut engine = empty_engine()?;
    if let Err(message) = apply(&mut engine, file.clone()) {
        // Applied one at a time, the first to fail is the one to blame.
        let mut alone = empty_engine()?;
        let blamed = items.iter().position(|item| {
            let one = json!({"format": SCHEMA_FORMAT, "collections": [item]});
            apply(&mut alone, one).is_err()
        });
        let location = match blamed {
            Some(i) => place(
                i,
                items[i].get("name").and_then(Value::as_str).unwrap_or(""),
            ),
            None => "collections".to_owned(),
        };
        r.error(location, message);
        return Ok(None);
    }
    let exported = engine.handle(Request::get("/api/schema").owner());
    let applied: Vec<Collection> = exported.json_body()["collections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();
    Ok(Some(
        defs.into_iter()
            .filter_map(|(i, d)| {
                let c = applied.iter().find(|c| c.name == d.name)?;
                Some((i, c.clone()))
            })
            .collect(),
    ))
}

/// The rules of a collection, by their key in a schema file.
fn rules_of(c: &Collection) -> [(&'static str, &str); 6] {
    let r = &c.rules;
    [
        ("listRule", &r.list_rule),
        ("viewRule", &r.view_rule),
        ("createRule", &r.create_rule),
        ("updateRule", &r.update_rule),
        ("deleteRule", &r.delete_rule),
        ("subscribeRule", &r.subscribe_rule),
    ]
}

/// Reports every relation whose target is none of `all`.
fn relation_targets(r: &mut Report, all: &[(usize, Collection)]) {
    for (i, c) in all {
        let at = place(*i, &c.name);
        for (j, f) in c.fields.iter().enumerate() {
            if let Some(rel) = f
                .relation
                .as_ref()
                .filter(|_| f.kind == FieldType::Relation)
                && !all.iter().any(|(_, o)| o.name == rel.target_moul)
            {
                r.error(
                    format!("{at}.fields[{j}] ({})", f.name),
                    format!("relation target {:?} is not a collection", rel.target_moul),
                );
            }
        }
    }
}

/// The checks on collections as an engine holds them, by their place in
/// the file.
fn check_collections(r: &mut Report, all: &[(usize, Collection)]) {
    let load = |name: &str| {
        all.iter()
            .find(|(_, c)| c.name == name)
            .map(|(_, c)| c.clone())
    };
    relation_targets(r, all);
    for (i, c) in all {
        let at = place(*i, &c.name);
        for (key, rule) in rules_of(c) {
            let where_ = format!("{at}.rules.{key}");
            if key == "listRule" {
                // As a list compiles it for everyone but the owner. A
                // blank list rule lets everyone list.
                if rule.trim().is_empty() {
                    continue;
                }
                let opts = FilterOptions {
                    param_prefix: "rule_",
                    untrusted: false,
                    load: Some(&load),
                    now_ms: 0,
                    cloak_hash: None,
                };
                if let Err(e) = moul_rules::filter_sql(rule, c, None, None, &opts) {
                    r.error(where_, e.message());
                }
            } else if !rule.is_empty() {
                // Evaluated against one record, `@request.body` included.
                for e in moul_rules::record_rule_names(rule, c, Some(&load)) {
                    r.error(where_.clone(), e.message());
                }
            }
        }
    }
    // Who may write without signing in. An auth collection's empty list,
    // view, update and delete rules become "only your own account" when
    // applied, but a Go engine database may still hold empty ones.
    for (i, c) in all {
        let at = place(*i, &c.name);
        let name = &c.name;
        let auth = c.kind == CollectionKind::Auth;
        let rs = &c.rules;
        let mut open = |key: &str, what: String| {
            r.warn(
                format!("{at}.rules.{key}"),
                format!("anyone can {what}, guests included: set {key}"),
            );
        };
        if auth && rs.list_rule.trim().is_empty() {
            open("listRule", format!("list the accounts in {name}"));
        }
        if auth && rs.view_rule.is_empty() {
            open("viewRule", format!("view the accounts in {name}"));
        }
        if rs.create_rule.is_empty() {
            let what = if auth {
                "create accounts in"
            } else {
                "create records in"
            };
            open("createRule", format!("{what} {name}"));
        }
        if rs.update_rule.is_empty() {
            open("updateRule", format!("change records of {name}"));
        }
        if rs.delete_rule.is_empty() {
            open("deleteRule", format!("delete records of {name}"));
        }
    }
}
