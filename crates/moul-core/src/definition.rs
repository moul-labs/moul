//! The schema file and TypeScript types.
//!
//! The schema file is the app definition (collections, fields with their
//! ids, rules) as one JSON document, for git, review and agents. Applying it
//! is a definition request like any other: collections are matched by id,
//! then by name, and created or changed, all in one transaction and one
//! change-log row.
//!
//! ```json
//! { "format": 1, "collections": [ { "id": "…", "name": "posts", "type": "base",
//!   "fields": [ { "id": "fld_…", "name": "title", "type": "text" } ],
//!   "rules": { "listRule": "", … } } ] }
//! ```

use moul_store::schema::{Collection, CollectionKind, Field, FieldType};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Response};
use crate::collections;
use crate::engine::Cx;

/// The version of the schema file format this engine writes and reads.
pub const SCHEMA_FORMAT: i64 = 1;

fn definition_json(c: &Collection) -> Value {
    let mut v = c.to_json();
    if let Some(o) = v.as_object_mut() {
        o.remove("createdAt");
        o.remove("updatedAt");
        o.remove("webhooks");
    }
    v
}

/// The schema file for `collections`, sorted by name so it diffs well.
pub fn schema_file(collections: &[Collection]) -> Value {
    let mut sorted: Vec<&Collection> = collections.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    json!({
        "format": SCHEMA_FORMAT,
        "collections": sorted.into_iter().map(definition_json).collect::<Vec<_>>(),
    })
}

/// What applying a schema file changed: collection names by outcome.
#[derive(Debug, Default, serde::Serialize)]
pub struct Applied {
    /// Collections made.
    pub created: Vec<String>,
    /// Collections changed.
    pub updated: Vec<String>,
    /// Collections removed (only with `prune`).
    pub deleted: Vec<String>,
    /// Collections already as described.
    pub unchanged: Vec<String>,
}

fn same_definition(a: &Collection, b: &Collection) -> bool {
    definition_json(a) == definition_json(b)
}

/// Applies a schema file: creates and changes collections to match it.
/// With `prune`, collections the file does not list are deleted.
pub(crate) fn apply(cx: &mut Cx<'_>, file: &Value, prune: bool) -> ApiResult<Applied> {
    let format = file
        .get("format")
        .and_then(Value::as_i64)
        .unwrap_or(SCHEMA_FORMAT);
    if format > SCHEMA_FORMAT {
        return Err(ApiError::bad_request(format!(
            "the schema file has format {format}; this engine reads format {SCHEMA_FORMAT} and older"
        )));
    }
    let Some(items) = file.get("collections").and_then(Value::as_array) else {
        return Err(ApiError::bad_request(
            "the schema file needs a \"collections\" array",
        ));
    };
    let mut wanted: Vec<Collection> = items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            serde_json::from_value(v.clone())
                .map_err(|e| ApiError::bad_request(format!("collections[{i}]: {e}")))
        })
        .collect::<ApiResult<_>>()?;
    let stored = cx.collections()?;
    let matching = |w: &Collection| {
        stored
            .iter()
            .find(|c| {
                (!w.id.is_empty() && c.id == w.id) || (w.id.is_empty() && c.name == w.name.trim())
            })
            .cloned()
    };
    // Every rename the file makes, as one map from old name to new. A
    // relation target in the file that names no collection the file
    // defines, but the old name of a renamed one, follows the rename; a
    // target the file states with a name it defines stays as written.
    let renames: std::collections::HashMap<String, String> = wanted
        .iter()
        .filter_map(|w| {
            let old = matching(w)?;
            (old.name != w.name.trim()).then(|| (old.name, w.name.trim().to_owned()))
        })
        .collect();
    let finals: std::collections::HashSet<String> =
        wanted.iter().map(|w| w.name.trim().to_owned()).collect();
    for (i, w) in wanted.iter_mut().enumerate() {
        for f in &mut w.fields {
            if let Some(rel) = f.relation.as_mut() {
                let t = rel.target_moul.trim().to_owned();
                if !finals.contains(&t)
                    && let Some(n) = renames.get(&t)
                {
                    rel.target_moul = n.clone();
                }
            }
        }
        // A null rule keeps the stored one, as a PATCH does.
        if let (Some(old), Some(Value::Object(raw))) = (matching(w), items[i].get("rules")) {
            let mut rules = serde_json::to_value(&w.rules).expect("rules serialise");
            let stored_rules = serde_json::to_value(&old.rules).expect("rules serialise");
            for (k, v) in raw {
                if v.is_null() {
                    rules[k] = stored_rules
                        .get(k)
                        .cloned()
                        .unwrap_or(Value::String(String::new()));
                }
            }
            w.rules = serde_json::from_value(rules).expect("rules round-trip");
        }
    }
    let mut applied = Applied::default();
    let mut touched = Vec::new();
    let mut kept_ids = Vec::new();
    // Creating in file order lets later collections point at earlier ones;
    // relation targets are names and are not checked at definition time.
    for want in wanted {
        let existing = cx.collections()?.into_iter().find(|c| {
            (!want.id.is_empty() && c.id == want.id) || (want.id.is_empty() && c.name == want.name)
        });
        match existing {
            None => {
                let made = collections::create_collection(cx, want)?;
                applied.created.push(made.name.clone());
                touched.push(made.id.clone());
                kept_ids.push(made.id);
            }
            Some(old) => {
                kept_ids.push(old.id.clone());
                let mut next = want;
                next.id = old.id.clone();
                next.webhooks = old.webhooks.clone();
                if next.kind != old.kind {
                    return Err(ApiError::bad_request(format!(
                        "cannot change collection type of {:?} from {:?} to {:?}",
                        old.name,
                        old.kind.as_str(),
                        next.kind.as_str()
                    )));
                }
                // Compare after the same cleaning a save would do.
                let mut probe = next.clone();
                probe
                    .validate(Some(&old))
                    .map_err(|e| ApiError::bad_request(e.0))?;
                let host = &mut *cx.host;
                probe.match_field_ids(Some(&old), || crate::ids::field_id(host));
                if same_definition(&probe, &old) {
                    applied.unchanged.push(old.name.clone());
                    continue;
                }
                let saved = collections::update_collection_with(cx, &old, probe, false)?;
                applied.updated.push(saved.name.clone());
                touched.push(saved.id);
            }
        }
    }
    // Collections the file does not list follow every rename at once.
    if !renames.is_empty() {
        let now = cx.now();
        for mut c in cx.collections()? {
            if kept_ids.contains(&c.id) {
                continue;
            }
            let mut moved = false;
            for f in &mut c.fields {
                if let Some(rel) = f.relation.as_mut()
                    && let Some(n) = renames.get(&rel.target_moul)
                {
                    rel.target_moul = n.clone();
                    moved = true;
                }
            }
            if moved {
                c.updated_at = now.clone();
                moul_store::catalog::save(cx.conn, &c)?;
                touched.push(c.id);
            }
        }
    }
    if prune {
        for c in cx.collections()? {
            if !kept_ids.contains(&c.id) {
                collections::delete_collection(cx, &c)?;
                applied.deleted.push(c.name.clone());
                touched.push(c.id);
            }
        }
    }
    cx.change = if touched.is_empty() {
        None
    } else {
        Some(crate::changes::ChangeInfo {
            collection: None,
            record: None,
            fields: touched,
            effects: Vec::new(),
        })
    };
    Ok(applied)
}

/// `GET /api/schema`: the schema file.
pub(crate) fn export_endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let all = cx.collections()?;
    Ok(Response::json(200, schema_file(&all)))
}

/// `POST /api/schema?prune=true`: applies a schema file.
pub(crate) fn apply_endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let body = match &cx.req.body {
        crate::api::Body::Json(v) => v.clone(),
        crate::api::Body::Bytes { data, .. } => serde_json::from_slice(data)
            .map_err(|_| ApiError::bad_request("Invalid request body"))?,
        _ => return Err(ApiError::bad_request("Invalid request body")),
    };
    let prune = cx.req.query_param("prune") == "true";
    let applied = apply(cx, &body, prune)?;
    Ok(Response::json(
        200,
        serde_json::to_value(applied).expect("serialises"),
    ))
}

/// `GET /api/types`: TypeScript types for the schema.
pub(crate) fn types_endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let all = cx.collections()?;
    Ok(Response::bytes(
        200,
        "text/plain; charset=utf-8",
        typescript(&all).into_bytes(),
    ))
}

fn pascal_case(s: &str) -> String {
    s.split(['_', '-', ' ', '.'])
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut ch = p.chars();
            match ch.next() {
                Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn ts_string(s: &str) -> String {
    serde_json::to_string(s).expect("strings serialise")
}

fn ts_type(f: &Field) -> String {
    match &f.kind {
        FieldType::Text
        | FieldType::Date
        | FieldType::Datetime
        | FieldType::Url
        | FieldType::Email
        | FieldType::Cloak => "string".into(),
        FieldType::Number => "number".into(),
        FieldType::Bool => "boolean".into(),
        FieldType::Json => "unknown".into(),
        FieldType::File => "MoulFile | MoulFile[] | null".into(),
        FieldType::Relation if f.relation.as_ref().is_some_and(|r| r.is_many()) => {
            "string[]".into()
        }
        FieldType::Relation => "string".into(),
        FieldType::Select if !f.options.is_empty() => f
            .options
            .iter()
            .map(|o| ts_string(o))
            .collect::<Vec<_>>()
            .join(" | "),
        FieldType::Select => "string".into(),
        FieldType::Unknown(_) => "unknown".into(),
    }
}

/// TypeScript types for `collections`, as `moul typegen` writes them. Port
/// of the Go engine's generator, matched to what the API returns: no
/// credential fields, relation lists as `string[]`, expanded relations
/// under `expand`.
pub fn typescript(collections: &[Collection]) -> String {
    let mut out = String::from(
        "/**\n * Generated by `moul typegen` from the app's schema.\n * Do not edit by hand: run `moul typegen` again instead.\n */\n\n",
    );
    out += "export interface BaseSystemFields {\n  id: string;\n  createdAt: string;\n  updatedAt: string;\n}\n\n";
    out += "export interface AuthSystemFields extends BaseSystemFields {\n  username: string;\n  /** Present for the record's owner, admins, or when emailVisibility is on. */\n  email?: string;\n}\n\n";
    out += "export type WorkerJobState = 'available' | 'executing' | 'completed' | 'discarded' | 'cancelled';\n\n";
    out += "export interface WorkerSystemFields extends BaseSystemFields {\n  state: WorkerJobState;\n  queue: string;\n  worker: string;\n  args: Record<string, unknown>;\n  meta: Record<string, unknown>;\n  tags: string[];\n  errors: string[];\n  attempt: number;\n  max_attempts: number;\n  priority: number;\n  inserted_at: string;\n  scheduled_at: string;\n  attempted_at?: string | null;\n  attempted_by?: string | null;\n  completed_at?: string | null;\n  discarded_at?: string | null;\n  cancelled_at?: string | null;\n}\n\n";
    out += "export interface AnalyticSystemFields extends BaseSystemFields {\n  visit_token: string;\n  visitor_token: string;\n  user_id?: string | null;\n  name: string;\n  properties: Record<string, unknown>;\n  time: string;\n}\n\n";
    out += "export interface MoulFile {\n  id: string;\n  url: string;\n  filename?: string;\n  size?: number;\n  createdAt?: string;\n}\n\n";

    let mut sorted: Vec<&Collection> = collections.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for c in &sorted {
        let name = format!("{}Record", pascal_case(&c.name));
        let base = match c.kind {
            CollectionKind::Auth => "AuthSystemFields",
            CollectionKind::Worker => "WorkerSystemFields",
            CollectionKind::Analytic => "AnalyticSystemFields",
            CollectionKind::Base => "BaseSystemFields",
        };
        out += &format!("export interface {name} extends {base} {{\n");
        let mut fields: Vec<&Field> = c.fields.iter().collect();
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        let mut expands = Vec::new();
        for f in fields {
            let optional = !f.required && f.kind != FieldType::Bool;
            out += &format!(
                "  {}{}: {};\n",
                f.name,
                if optional { "?" } else { "" },
                ts_type(f)
            );
            if let Some(rel) = f.relation.as_ref().filter(|r| !r.target_moul.is_empty()) {
                let target = format!("{}Record", pascal_case(&rel.target_moul));
                let t = if rel.is_many() {
                    format!("{target}[]")
                } else {
                    format!("{target} | null")
                };
                expands.push(format!("    {}?: {t};\n", f.name));
            }
        }
        if !expands.is_empty() {
            out += "  expand?: {\n";
            for e in expands {
                out += &e;
            }
            out += "  };\n";
        }
        out += "}\n\n";
    }
    out += "export interface MoulSchema {\n";
    for c in &sorted {
        out += &format!(
            "  {}: {}Record;\n",
            ts_string(&c.name),
            pascal_case(&c.name)
        );
    }
    out += "}\n\nexport type MoulCollectionName = keyof MoulSchema;\n\nexport type RecordModel<T extends MoulCollectionName> = MoulSchema[T];\n";
    out
}

/// Reads a schema file's collections without applying them, checking each
/// definition. For `moul check` and tests.
pub fn read_schema_file(file: &Value) -> Result<Vec<Collection>, String> {
    let items = file
        .get("collections")
        .and_then(Value::as_array)
        .ok_or("the schema file needs a \"collections\" array")?;
    let mut out = Vec::new();
    for (i, v) in items.iter().enumerate() {
        let mut c: Collection =
            serde_json::from_value(v.clone()).map_err(|e| format!("collections[{i}]: {e}"))?;
        c.validate(None)
            .map_err(|e| format!("collections[{i}] ({}): {e}", c.name))?;
        out.push(c);
    }
    let _ = Map::<String, Value>::new();
    Ok(out)
}
