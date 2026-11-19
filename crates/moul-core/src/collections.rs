//! Definition requests: creating, changing and deleting collections. Port
//! of the Go engine's `MoulHandler`, with field ids and renames that keep
//! data.

use moul_store::catalog;
use moul_store::schema::{
    Collection, CollectionKind, EmailTemplates, FieldChanges, FieldType, INVALID_NAME, Rules,
    is_valid_collection_id,
};
use serde_json::{Map, Value};

use crate::api::{ApiError, ApiResult, Response};
use crate::engine::Cx;
use crate::ids;

fn not_found() -> ApiError {
    ApiError::not_found("Not found")
}

fn body_object(cx: &Cx<'_>) -> ApiResult<Map<String, Value>> {
    match cx.req.json_object() {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Ok(Map::new()),
        Err(()) => Err(ApiError::bad_request("Invalid request body")),
    }
}

/// `GET /api/moul`
pub(crate) fn list(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let all = cx.collections()?;
    Ok(Response::json(
        200,
        serde_json::to_value(all).expect("collections serialise"),
    ))
}

/// `GET /api/moul/:name`
pub(crate) fn get(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    cx.require_admin()?;
    let c = cx.collection(name)?.ok_or_else(not_found)?;
    Ok(Response::json(200, c.to_json()))
}

fn check_name_free(cx: &mut Cx<'_>, c: &Collection) -> ApiResult<()> {
    if c.name.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(ApiError::bad_request(INVALID_NAME));
    }
    let taken = cx
        .collections()?
        .iter()
        .any(|o| o.id != c.id && o.name.eq_ignore_ascii_case(&c.name));
    if taken || (catalog::table_exists(cx.conn, &c.name)? && cx.collection(&c.name)?.is_none()) {
        return Err(ApiError::bad_request(format!(
            "Moul with name {:?} already exists",
            c.name
        )));
    }
    Ok(())
}

/// Saves a new collection and creates its table. Shared by the HTTP
/// handler, imports of a schema file and tests. Its rules are not checked:
/// the caller checks them once every collection it saves is in place
/// ([`crate::rules::refuse_broken`]).
pub(crate) fn create_collection(cx: &mut Cx<'_>, mut c: Collection) -> ApiResult<Collection> {
    if !matches!(
        c.kind,
        CollectionKind::Auth | CollectionKind::Worker | CollectionKind::Analytic
    ) {
        c.kind = CollectionKind::Base;
    }
    c.validate(None).map_err(|e| ApiError::bad_request(e.0))?;
    if c.id.is_empty() {
        c.id = ids::collection_id(cx.host);
    } else if !is_valid_collection_id(&c.id) {
        return Err(ApiError::bad_request(
            "Collection id must be 1 to 64 letters, digits, '-' or '_'",
        ));
    }
    if cx.collections()?.iter().any(|o| o.id == c.id) {
        return Err(ApiError::bad_request(format!(
            "Collection id {:?} is taken",
            c.id
        )));
    }
    check_name_free(cx, &c)?;
    let host = &mut *cx.host;
    c.match_field_ids(None, || ids::field_id(host));
    let now = cx.now();
    c.created_at = now.clone();
    c.updated_at = now;
    catalog::create_table(cx.conn, &c)?;
    // Saving moves webhook secrets to the writer-only store; the answer
    // does not echo them either.
    catalog::save(cx.conn, &c)?;
    strip_webhook_secrets(&mut c);
    let fields = c.fields.iter().map(|f| f.id.clone()).collect();
    cx.changed(&c, None, fields);
    Ok(c)
}

/// `POST /api/moul`
pub(crate) fn create(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let body = body_object(cx)?;
    let c: Collection = serde_json::from_value(Value::Object(body))
        .map_err(|_| ApiError::bad_request("Invalid request body"))?;
    let c = create_collection(cx, c)?;
    crate::rules::refuse_broken(cx, std::slice::from_ref(&c.id), false)?;
    Ok(Response::json(201, c.to_json()))
}

/// Overlays the keys present in a PATCH body onto `c`, as Go's `moulPatch`.
fn apply_patch(c: &mut Collection, body: &Map<String, Value>) -> Result<(), String> {
    if let Some(name) = body.get("name").and_then(Value::as_str) {
        let name = name.trim();
        if !name.is_empty() {
            c.name = name.to_owned();
        }
    }
    if let Some(kind) = body.get("type").and_then(Value::as_str)
        && !kind.is_empty()
        && kind != c.kind.as_str()
    {
        return Err(format!(
            "cannot change collection type from {:?} to {:?}",
            c.kind.as_str(),
            kind
        ));
    }
    if let Some(fields) = body.get("fields") {
        c.fields = if fields.is_null() {
            Vec::new()
        } else {
            serde_json::from_value(fields.clone()).map_err(|_| "invalid fields".to_owned())?
        };
    }
    if let Some(rules) = body.get("rules") {
        // Keys left out keep their stored rule.
        let mut merged = serde_json::to_value(&c.rules).expect("rules serialise");
        match rules {
            Value::Object(m) => {
                for (k, v) in m {
                    if !v.is_string() && !v.is_null() {
                        return Err("invalid rules".into());
                    }
                    // As Go's decoder does, a null leaves the stored rule.
                    if !v.is_null() {
                        merged[k] = v.clone();
                    }
                }
            }
            Value::Null => {}
            _ => return Err("invalid rules".into()),
        }
        c.rules =
            serde_json::from_value::<Rules>(merged).map_err(|_| "invalid rules".to_owned())?;
    }
    if let Some(t) = body.get("emailTemplates") {
        c.email_templates = if t.is_null() {
            None
        } else {
            Some(
                serde_json::from_value::<EmailTemplates>(t.clone())
                    .map_err(|_| "invalid emailTemplates".to_owned())?,
            )
        };
    }
    if let Some(w) = body.get("webhooks") {
        c.webhooks = match w {
            Value::Null => Vec::new(),
            // Saving moves any secret to the writer-only store.
            Value::Array(items) => items.clone(),
            _ => return Err("invalid webhooks".into()),
        };
    }
    Ok(())
}

/// Replaces the definition of `old` with `new`: renames the table, moves
/// file references and relation targets with it, and changes columns by
/// field id. Returns the saved definition. Its rules are not checked, as in
/// [`create_collection`].
pub(crate) fn update_collection(
    cx: &mut Cx<'_>,
    old: &Collection,
    new: Collection,
) -> ApiResult<Collection> {
    update_collection_with(cx, old, new, true)
}

/// [`update_collection`]; `retarget` says whether relation fields of other
/// collections that name a renamed collection follow it. A schema file
/// sets it false and maps every rename at once instead.
pub(crate) fn update_collection_with(
    cx: &mut Cx<'_>,
    old: &Collection,
    mut new: Collection,
    retarget: bool,
) -> ApiResult<Collection> {
    new.id = old.id.clone();
    new.kind = old.kind;
    new.validate(Some(old))
        .map_err(|e| ApiError::bad_request(e.0))?;
    if new.name != old.name {
        check_name_free(cx, &new)?;
    }
    let host = &mut *cx.host;
    new.match_field_ids(Some(old), || ids::field_id(host));
    let now = cx.now();
    new.created_at = old.created_at.clone();
    new.updated_at = now.clone();
    catalog::sync_table(cx.conn, old, &new)?;
    let changes = FieldChanges::between(old, &new);
    if new.name != old.name {
        cx.conn.execute(
            "UPDATE main._file_refs SET moul = ?1 WHERE moul = ?2",
            [&new.name, &old.name],
        )?;
        // Uploads made by members of a renamed auth collection stay theirs.
        cx.conn.execute(
            "UPDATE main._files SET ownerMoul = ?1 WHERE ownerMoul = ?2",
            [&new.name, &old.name],
        )?;
        if retarget {
            catalog::retarget_relations(cx.conn, &old.name, &new.name, &now)?;
        }
        // The collection's own relations to itself follow it too.
        for f in &mut new.fields {
            if let Some(rel) = f.relation.as_mut()
                && rel.target_moul == old.name
            {
                rel.target_moul = new.name.clone();
            }
        }
    }
    // File references name the field. When a file field is renamed,
    // removed or retyped, rebuild the collection's references from its rows,
    // as Go's RebuildMoulRefs does: swaps and chains of names stay right.
    let is_file =
        |c: &Collection, id: &str| c.field_by_id(id).is_some_and(|f| f.kind == FieldType::File);
    let files_moved = changes.renamed.iter().any(|(id, _, _)| is_file(&new, id))
        || changes.removed.iter().any(|id| is_file(old, id))
        || changes
            .changed
            .iter()
            .any(|id| is_file(old, id) != is_file(&new, id));
    if files_moved {
        cx.conn
            .execute("DELETE FROM main._file_refs WHERE moul = ?1", [&new.name])?;
        let rows = crate::records::query(
            cx.conn,
            &format!("SELECT * FROM {}", moul_store::schema::quote(&new.name)),
            &[],
            None,
        )?;
        for mut r in rows {
            crate::values::normalize(&new, &mut r);
            crate::files::sync_refs(cx, &new, &r)?;
        }
    }
    // Secrets of webhooks the definition no longer lists go too.
    let kept: Vec<&str> = new
        .webhooks
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str))
        .collect();
    let mut stmt = cx
        .conn
        .prepare("SELECT webhook FROM writer._webhook_secrets WHERE collection = ?1")?;
    let stored: Vec<String> = stmt
        .query_map([&new.id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    for w in stored.iter().filter(|w| !kept.contains(&w.as_str())) {
        cx.conn.execute(
            "DELETE FROM writer._webhook_secrets WHERE collection = ?1 AND webhook = ?2",
            [&new.id, w],
        )?;
    }
    catalog::save(cx.conn, &new)?;
    strip_webhook_secrets(&mut new);
    let mut fields: Vec<String> = changes.added;
    fields.extend(changes.removed);
    fields.extend(changes.renamed.into_iter().map(|(id, _, _)| id));
    fields.extend(changes.changed);
    cx.changed(&new, None, fields);
    Ok(new)
}

/// `PATCH|PUT /api/moul/:name`
pub(crate) fn update(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    cx.require_admin()?;
    if name.starts_with('_') {
        return Err(ApiError::bad_request("Cannot update system collections"));
    }
    let old = cx.collection(name)?.ok_or_else(not_found)?;
    let body = body_object(cx)?;
    let mut new = old.clone();
    apply_patch(&mut new, &body).map_err(ApiError::bad_request)?;
    let new = update_collection(cx, &old, new)?;
    crate::rules::refuse_broken(cx, std::slice::from_ref(&new.id), false)?;
    Ok(Response::json(200, new.to_json()))
}

/// Removes webhook secrets from a definition held in memory, after
/// [`catalog::save`] moved them to the writer-only store.
fn strip_webhook_secrets(c: &mut Collection) {
    for hook in &mut c.webhooks {
        if let Some(o) = hook.as_object_mut() {
            o.retain(|k, _| !k.eq_ignore_ascii_case("secret"));
        }
    }
}

/// Drops a collection, its table and its file references.
pub(crate) fn delete_collection(cx: &mut Cx<'_>, c: &Collection) -> ApiResult<()> {
    catalog::drop_table(cx.conn, c)?;
    catalog::delete(cx.conn, &c.id)?;
    cx.conn
        .execute("DELETE FROM main._file_refs WHERE moul = ?1", [&c.name])?;
    cx.conn
        .execute("DELETE FROM writer._auth WHERE collection = ?1", [&c.id])?;
    cx.conn.execute(
        "DELETE FROM writer._webhook_secrets WHERE collection = ?1",
        [&c.id],
    )?;
    cx.changed(c, None, Vec::new());
    Ok(())
}

/// `DELETE /api/moul/:name`
pub(crate) fn delete(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    cx.require_admin()?;
    if name.starts_with('_') {
        return Err(ApiError::bad_request("Cannot delete system collections"));
    }
    let c = cx.collection(name)?.ok_or_else(not_found)?;
    delete_collection(cx, &c)?;
    Ok(Response::empty(204))
}

fn auth_collection(cx: &mut Cx<'_>, name: &str) -> ApiResult<Collection> {
    let c = cx.collection(name)?.ok_or_else(not_found)?;
    if c.kind != CollectionKind::Auth {
        return Err(ApiError::bad_request("This moul is not an auth collection"));
    }
    Ok(c)
}

/// `GET /api/moul/:name/email-templates`
pub(crate) fn get_templates(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    cx.require_admin()?;
    let c = auth_collection(cx, name)?;
    let t = c.email_templates.unwrap_or_default();
    Ok(Response::json(
        200,
        serde_json::to_value(t).expect("templates serialise"),
    ))
}

/// `PUT /api/moul/:name/email-templates`
pub(crate) fn put_templates(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    cx.require_admin()?;
    let mut c = auth_collection(cx, name)?;
    let body = body_object(cx)?;
    let t: EmailTemplates = serde_json::from_value(Value::Object(body))
        .map_err(|_| ApiError::bad_request("Invalid request body"))?;
    c.email_templates = Some(t.clone());
    c.updated_at = cx.now();
    catalog::save(cx.conn, &c)?;
    cx.changed(&c, None, Vec::new());
    Ok(Response::json(
        200,
        serde_json::to_value(t).expect("templates serialise"),
    ))
}
