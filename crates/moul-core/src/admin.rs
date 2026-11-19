//! Root users: the administrators of a native server, kept in the
//! writer-only store's `_rootUsers`. Port of the Go engine's setup and
//! admin-login endpoints.
//!
//! | Path | Methods | |
//! | --- | --- | --- |
//! | `/api/setup` | `GET`, `POST` | whether a root user exists; make the first |
//! | `/api/admin/login` | `POST` | sign in as a root user |
//! | `/api/setup/account`, `/api/admin/account`, `/api/settings/account` | `GET`, `POST`, `PATCH` | read or change a root account |
//! | `/api/setup/password`, `/api/admin/password`, `/api/settings/password` | `POST`, `PATCH` | change a root password |
//!
//! All of them need the admin key (or the host's owner); a root user's token
//! alone is not enough, as in Go. With both, the account endpoints act on
//! the token's root user.

use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Response};
use crate::auth::{ROOT_COLLECTION, issue_token};
use crate::engine::{Cx, Who};
use crate::{crypto, ids, records, settings};

fn body(cx: &Cx<'_>) -> ApiResult<Map<String, Value>> {
    match cx.req.json_object() {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Ok(Map::new()),
        Err(()) => Err(ApiError::bad_request("Invalid request body")),
    }
}

fn text(body: &Map<String, Value>, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned()
}

fn count(cx: &Cx<'_>) -> ApiResult<i64> {
    Ok(cx
        .conn
        .query_row("SELECT count(*) FROM writer._rootUsers", [], |r| r.get(0))?)
}

/// `GET /api/setup`
pub(crate) fn status(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin_key()?;
    Ok(Response::json(200, json!({"needsSetup": count(cx)? == 0})))
}

/// `POST /api/setup`: makes the first root user.
pub(crate) fn setup(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin_key()?;
    if count(cx)? > 0 {
        return Err(ApiError::bad_request("Root user setup is already complete"));
    }
    let b = body(cx)?;
    let (username, email, password) = (
        text(&b, "username"),
        text(&b, "email"),
        text(&b, "password"),
    );
    let mut name = text(&b, "name");
    if username.is_empty() || email.is_empty() || password.is_empty() {
        return Err(ApiError::bad_request(
            "username, email, and password are required",
        ));
    }
    if password.len() < 8 {
        return Err(ApiError::bad_request(
            "password must be at least 8 characters long",
        ));
    }
    if !email.contains('@') {
        return Err(ApiError::bad_request("invalid email address"));
    }
    if name.is_empty() {
        name = username.clone();
    }
    let id = ids::random_chars(cx.host, 15);
    let hash = crypto::hash_password(cx.host, &password);
    let now = cx.now();
    cx.conn.execute(
        "INSERT INTO writer._rootUsers (id, username, email, name, passwordHash, createdAt, updatedAt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![id, username, email, name, hash, now],
    )?;
    Ok(Response::json(
        200,
        json!({"message": "Root user created successfully", "id": id}),
    ))
}

fn by_identity(cx: &Cx<'_>, identity: &str) -> ApiResult<Option<Map<String, Value>>> {
    Ok(records::query(
        cx.conn,
        "SELECT * FROM writer._rootUsers WHERE username = :v OR email = :v LIMIT 1",
        &[("v".into(), rusqlite::types::Value::Text(identity.into()))],
        None,
    )?
    .into_iter()
    .next())
}

/// The root user an account request is about: the root token's, else the
/// one `identity` names, else the oldest.
fn target(cx: &Cx<'_>, identity: &str) -> ApiResult<Map<String, Value>> {
    let rows = if let Who::Owner { root: Some(u) } = &cx.who {
        records::query(
            cx.conn,
            "SELECT * FROM writer._rootUsers WHERE id = :v",
            &[("v".into(), rusqlite::types::Value::Text(u.id.clone()))],
            None,
        )?
    } else if !identity.is_empty() {
        by_identity(cx, identity)?.into_iter().collect()
    } else {
        records::query(
            cx.conn,
            "SELECT * FROM writer._rootUsers ORDER BY createdAt ASC LIMIT 1",
            &[],
            None,
        )?
    };
    rows.into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found("Root user not found"))
}

fn s(row: &Map<String, Value>, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn record_json(row: &Map<String, Value>) -> Value {
    let mut name = s(row, "name");
    if name.is_empty() {
        name = s(row, "username");
    }
    json!({
        "id": s(row, "id"),
        "email": s(row, "email"),
        "username": s(row, "username"),
        "name": name,
        "moul": ROOT_COLLECTION,
        "createdAt": row.get("createdAt").cloned().unwrap_or(Value::Null),
        "updatedAt": row.get("updatedAt").cloned().unwrap_or(Value::Null),
    })
}

/// `POST /api/admin/login`
pub(crate) fn login(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin_key()?;
    let b = body(cx)?;
    let (identity, password) = (text(&b, "identity"), text(&b, "password"));
    if identity.is_empty() || password.is_empty() {
        return Err(ApiError::bad_request(
            "username/email and password are required",
        ));
    }
    let Some(row) = by_identity(cx, &identity)? else {
        return Err(ApiError::bad_request("Invalid email/username or password"));
    };
    if !crypto::verify_password(&s(&row, "passwordHash"), &password) {
        return Err(ApiError::bad_request("Invalid email/username or password"));
    }
    if !settings::root_ip_allowed(cx.conn, cx.req.remote.as_deref()) {
        return Err(ApiError::forbidden(
            "Your IP address is not authorized to log in as a root user",
        ));
    }
    let token = issue_token(
        cx,
        &s(&row, "id"),
        &s(&row, "email"),
        &s(&row, "username"),
        ROOT_COLLECTION,
    );
    Ok(Response::json(
        200,
        json!({"token": token, "record": record_json(&row)}),
    ))
}

/// `GET /api/admin/account`
pub(crate) fn account(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin_key()?;
    let row = target(cx, cx.req.query_param("identity").trim())?;
    let record = record_json(&row);
    let mut out = record.as_object().cloned().unwrap_or_default();
    out.insert("record".into(), record);
    Ok(Response::json(200, Value::Object(out)))
}

/// Checks a password change: the current password is given and right, the
/// new one is confirmed and strong enough. Returns the new hash.
fn new_password_hash(
    cx: &mut Cx<'_>,
    row: &Map<String, Value>,
    current: &str,
    new: &str,
    confirm: &str,
    current_missing: &str,
) -> ApiResult<String> {
    if current.is_empty() {
        return Err(ApiError::bad_request(current_missing));
    }
    if new.is_empty() {
        return Err(ApiError::bad_request("password is required"));
    }
    if !confirm.is_empty() && new != confirm {
        return Err(ApiError::bad_request(
            "password and passwordConfirm must match",
        ));
    }
    crypto::validate_password(new).map_err(ApiError::bad_request)?;
    if !crypto::verify_password(&s(row, "passwordHash"), current) {
        return Err(ApiError::bad_request("Invalid current password"));
    }
    Ok(crypto::hash_password(cx.host, new))
}

/// `POST|PATCH /api/admin/account`: changes the username, name, email or
/// password. An empty value means no change.
pub(crate) fn update_account(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin_key()?;
    let b = body(cx)?;
    let mut identity = text(&b, "identity");
    if identity.is_empty() {
        identity = cx.req.query_param("identity").trim().to_owned();
    }
    let row = target(cx, &identity)?;
    let id = s(&row, "id");
    let mut sets: Vec<(&str, String)> = Vec::new();

    let username = text(&b, "username");
    let raw_username = b.get("username").and_then(Value::as_str).unwrap_or("");
    if !raw_username.is_empty() && username.is_empty() {
        return Err(ApiError::bad_request("username cannot be empty"));
    }
    if !username.is_empty() && username != s(&row, "username") {
        let taken: Option<i64> = cx
            .conn
            .query_row(
                "SELECT 1 FROM writer._rootUsers WHERE username = ?1 AND id != ?2",
                params![username, id],
                |r| r.get(0),
            )
            .optional()?;
        if taken.is_some() {
            return Err(ApiError::bad_request("Username is already taken"));
        }
        sets.push(("username", username));
    }
    let name = text(&b, "name");
    let mut current_name = s(&row, "name");
    if current_name.is_empty() {
        current_name = s(&row, "username");
    }
    if !name.is_empty() && name != current_name {
        sets.push(("name", name));
    }
    let email = text(&b, "email");
    if b.get("email")
        .and_then(Value::as_str)
        .is_some_and(|e| !e.is_empty())
    {
        if !email.contains('@') {
            return Err(ApiError::bad_request("invalid email address"));
        }
        if email != s(&row, "email") {
            let taken: Option<i64> = cx
                .conn
                .query_row(
                    "SELECT 1 FROM writer._rootUsers WHERE email = ?1 AND id != ?2",
                    params![email, id],
                    |r| r.get(0),
                )
                .optional()?;
            if taken.is_some() {
                return Err(ApiError::bad_request("Email is already in use"));
            }
            sets.push(("email", email));
        }
    }
    let mut new = text(&b, "password");
    if new.is_empty() {
        new = text(&b, "newPassword");
    }
    let current = text(&b, "currentPassword");
    if !new.is_empty() || !current.is_empty() {
        let hash = new_password_hash(
            cx,
            &row,
            &current,
            &new,
            &text(&b, "passwordConfirm"),
            "currentPassword is required to change password",
        )?;
        sets.push(("passwordHash", hash));
    }
    if sets.iter().any(|(c, _)| *c == "passwordHash") {
        forget_refresh_tokens(cx, &id)?;
    }
    if !sets.is_empty() {
        sets.push(("updatedAt", cx.now()));
        for (col, value) in &sets {
            cx.conn.execute(
                &format!("UPDATE writer._rootUsers SET {col} = ?1 WHERE id = ?2"),
                params![value, id],
            )?;
        }
    }
    let row = target_by_id(cx, &id)?;
    let token = issue_token(
        cx,
        &id,
        &s(&row, "email"),
        &s(&row, "username"),
        ROOT_COLLECTION,
    );
    Ok(Response::json(
        200,
        json!({
            "message": "Root account updated successfully",
            "token": token,
            "record": record_json(&row),
        }),
    ))
}

fn target_by_id(cx: &Cx<'_>, id: &str) -> ApiResult<Map<String, Value>> {
    records::query(
        cx.conn,
        "SELECT * FROM writer._rootUsers WHERE id = :v",
        &[("v".into(), rusqlite::types::Value::Text(id.into()))],
        None,
    )?
    .into_iter()
    .next()
    .ok_or_else(|| ApiError::not_found("Root user not found"))
}

/// `POST|PATCH /api/admin/password`
pub(crate) fn update_password(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin_key()?;
    let b = body(cx)?;
    let mut current = text(&b, "currentPassword");
    if current.is_empty() {
        current = text(&b, "oldPassword");
    }
    let mut new = text(&b, "password");
    if new.is_empty() {
        new = text(&b, "newPassword");
    }
    let confirm = text(&b, "passwordConfirm");
    // Go checks the fields before it looks the user up.
    if current.is_empty() {
        return Err(ApiError::bad_request("currentPassword is required"));
    }
    if new.is_empty() {
        return Err(ApiError::bad_request("password is required"));
    }
    if !confirm.is_empty() && new != confirm {
        return Err(ApiError::bad_request(
            "password and passwordConfirm must match",
        ));
    }
    crypto::validate_password(&new).map_err(ApiError::bad_request)?;
    let row = target(cx, &text(&b, "identity"))?;
    let hash = new_password_hash(
        cx,
        &row,
        &current,
        &new,
        &confirm,
        "currentPassword is required",
    )?;
    cx.conn.execute(
        "UPDATE writer._rootUsers SET passwordHash = ?1, updatedAt = ?2 WHERE id = ?3",
        params![hash, cx.now(), s(&row, "id")],
    )?;
    forget_refresh_tokens(cx, &s(&row, "id"))?;
    Ok(Response::json(
        200,
        json!({"message": "Root password updated successfully"}),
    ))
}

/// Ends the MCP clients' refresh tokens of a root user whose password
/// changed: an agent approved before must be approved again.
fn forget_refresh_tokens(cx: &Cx<'_>, root: &str) -> ApiResult<()> {
    cx.conn.execute(
        "DELETE FROM writer._oauth_refresh WHERE rootUser = ?1",
        [root],
    )?;
    Ok(())
}
