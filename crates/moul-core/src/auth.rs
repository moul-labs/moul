//! Auth: who a request comes from, signing in with a password or an email
//! code, refreshing and revoking tokens, resetting a password, and the local
//! inbox where mail lands instead of being sent.
//!
//! Credentials live in the writer-only store's `_auth` table, keyed by
//! collection id and record id. The Go engine kept them in the auth table
//! itself; a converted database has them moved.

use std::collections::HashMap;

use moul_store::schema::{Collection, CollectionKind, quote};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Caller, Request, Response};
use crate::crypto::{self, Claims};
use crate::engine::{Config, Cx, Who};
use crate::realtime::{Action, RecordEvent};
use crate::{ids, records, time, values};

/// A person signed in to an auth collection, as their token says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    /// Record id.
    pub id: String,
    /// Email.
    pub email: String,
    /// Username.
    pub username: String,
    /// The auth collection's name, or `_rootUsers` for an administrator.
    pub collection: String,
}

impl User {
    /// The map rules see as `@request.auth`.
    pub fn map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("email".into(), json!(self.email));
        m.insert("username".into(), json!(self.username));
        m.insert("moul".into(), json!(self.collection));
        m
    }
}

/// The root (administrator) collection of the Go engine.
pub const ROOT_COLLECTION: &str = "_rootUsers";

fn bearer(req: &Request) -> Option<&str> {
    let h = req.header_value("Authorization");
    let (scheme, token) = h.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && !token.contains(' ') {
        Some(token)
    } else {
        None
    }
}

/// Whether the request carries the admin key, as Go's `CheckAdminKey`:
/// `X-Admin-Key`, or the `Authorization` header as a bearer token, raw, or
/// as the password of HTTP Basic auth.
fn has_admin_key(req: &Request, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let eq = |s: &str| crypto::constant_time_eq(s.as_bytes(), key.as_bytes());
    let x = req.header_value("X-Admin-Key");
    if !x.is_empty() && eq(x) {
        return true;
    }
    let h = req.header_value("Authorization");
    if h.is_empty() {
        return false;
    }
    let mut token = h;
    if let Some((scheme, rest)) = h.split_once(' ') {
        if scheme.eq_ignore_ascii_case("bearer") {
            token = rest;
        } else if scheme.eq_ignore_ascii_case("basic")
            && let Some(decoded) = crypto::base64_decode(rest)
            && let Ok(pair) = String::from_utf8(decoded)
            && let Some((_, secret)) = pair.split_once(':')
            && eq(secret)
        {
            return true;
        }
    }
    eq(token)
}

fn token_hash(token: &str) -> String {
    crypto::sha256_hex(token.as_bytes())
}

fn is_revoked(conn: &Connection, token: &str, now_ms: i64) -> bool {
    conn.query_row(
        "SELECT expires_at FROM writer._revoked_tokens WHERE token_hash = ?1",
        [token_hash(token)],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .is_some_and(|exp| time::parse_rfc3339(&exp).is_none_or(|e| e > now_ms))
}

fn revoke(conn: &Connection, token: &str, expires_ms: i64, now_ms: i64) -> rusqlite::Result<()> {
    // Rows past their expiry protect nothing: drop them as new ones come.
    conn.execute(
        "DELETE FROM writer._revoked_tokens WHERE expires_at < ?1",
        [time::rfc3339(now_ms)],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO writer._revoked_tokens (token_hash, expires_at) VALUES (?1, ?2)",
        params![token_hash(token), time::rfc3339(expires_ms)],
    )?;
    Ok(())
}

/// Works out who sent `req`. Port of the Go engine's auth middleware: an
/// invalid, expired or revoked token leaves the request a guest, marked as
/// rejected.
pub(crate) fn identify(
    conn: &Connection,
    config: &Config,
    key: &[u8],
    req: &Request,
    now_ms: i64,
) -> Result<Who, ApiError> {
    if req.caller == Caller::Owner {
        return Ok(Who::Owner { root: None });
    }
    if let Some(admin) = &config.admin_key
        && has_admin_key(req, admin)
    {
        return Ok(Who::Owner { root: None });
    }
    let Some(token) = bearer(req) else {
        return Ok(Who::Guest { rejected: false });
    };
    if is_revoked(conn, token, now_ms) {
        return Ok(Who::Guest { rejected: true });
    }
    let Ok(claims) = crypto::verify_token(key, token, Some(now_ms.div_euclid(1000))) else {
        return Ok(Who::Guest { rejected: true });
    };
    let user = User {
        id: claims.id,
        email: claims.email,
        username: claims.username,
        collection: claims.collection,
    };
    if user.collection == ROOT_COLLECTION {
        // Root users may be held to a list of addresses.
        if !crate::settings::root_ip_allowed(conn, req.remote.as_deref()) {
            return Err(ApiError::forbidden("IP address not authorized"));
        }
        return Ok(Who::Owner { root: Some(user) });
    }
    Ok(Who::User(user))
}

/// Signs a token for a record of `collection`.
pub(crate) fn issue_token(
    cx: &mut Cx<'_>,
    id: &str,
    email: &str,
    username: &str,
    collection: &str,
) -> String {
    let now_s = cx.now_ms.div_euclid(1000);
    let claims = Claims {
        id: id.into(),
        email: email.into(),
        username: username.into(),
        collection: collection.into(),
        exp: Some(now_s + cx.config.token_ttl_ms / 1000),
        iat: Some(now_s),
        jti: ids::random_hex(cx.host, 16),
    };
    crypto::sign_token(cx.key, &claims)
}

/// Sets one credential column of record `record` in `writer._auth`.
pub(crate) fn set_secret(
    conn: &Connection,
    collection_id: &str,
    record: &str,
    column: &str,
    value: Option<&str>,
) -> ApiResult<()> {
    const ALLOWED: [&str; 9] = [
        "passwordHash",
        "tokenKey",
        "otpCode",
        "otpExpiresAt",
        "otpAttempts",
        "passkeys",
        "resetToken",
        "resetTokenExpiresAt",
        "oauthProviders",
    ];
    assert!(
        ALLOWED.contains(&column),
        "not a credential column: {column}"
    );
    conn.execute(
        &format!(
            "INSERT INTO writer._auth (collection, record, {c}) VALUES (?1, ?2, ?3)
             ON CONFLICT (collection, record) DO UPDATE SET {c} = excluded.{c}",
            c = quote(column)
        ),
        params![collection_id, record, value],
    )?;
    Ok(())
}

pub(crate) fn secret(
    conn: &Connection,
    collection_id: &str,
    record: &str,
    column: &str,
) -> ApiResult<Option<String>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {} FROM writer._auth WHERE collection = ?1 AND record = ?2",
                quote(column)
            ),
            params![collection_id, record],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

pub(crate) fn auth_collection(cx: &mut Cx<'_>, name: &str) -> ApiResult<Collection> {
    let c = cx
        .collection(name)?
        .ok_or_else(|| ApiError::not_found("Not found"))?;
    if c.kind != CollectionKind::Auth {
        return Err(ApiError::bad_request("This moul is not an auth collection"));
    }
    Ok(c)
}

fn body(cx: &Cx<'_>) -> ApiResult<Map<String, Value>> {
    match cx.req.json_object() {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Ok(Map::new()),
        Err(()) => Err(ApiError::bad_request("Invalid request body")),
    }
}

fn str_field<'a>(body: &'a Map<String, Value>, key: &str) -> &'a str {
    body.get(key).and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn find_by(
    cx: &Cx<'_>,
    c: &Collection,
    sql_where: &str,
    value: &str,
) -> ApiResult<Option<values::Record>> {
    Ok(records::query(
        cx.conn,
        &format!("SELECT * FROM {} WHERE {sql_where} LIMIT 1", quote(&c.name)),
        &[("v".into(), rusqlite::types::Value::Text(value.into()))],
        None,
    )?
    .into_iter()
    .next())
}

pub(crate) fn rec_str(rec: &values::Record, key: &str) -> String {
    rec.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// The sign-in answer: a token and the record, as the Go engine sends it.
pub(crate) fn signed_in(cx: &mut Cx<'_>, c: &Collection, mut rec: values::Record) -> Response {
    let (id, email, username) = (
        rec_str(&rec, "id"),
        rec_str(&rec, "email"),
        rec_str(&rec, "username"),
    );
    let token = issue_token(cx, &id, &email, &username, &c.name);
    values::normalize(c, &mut rec);
    Response::json(200, json!({"token": token, "record": rec}))
}

fn root_signed_in(cx: &mut Cx<'_>, row: &Map<String, Value>) -> Response {
    let id = rec_str(row, "id");
    let email = rec_str(row, "email");
    let username = rec_str(row, "username");
    let mut name = rec_str(row, "name");
    if name.is_empty() {
        name = username.clone();
    }
    let token = issue_token(cx, &id, &email, &username, ROOT_COLLECTION);
    Response::json(
        200,
        json!({
            "token": token,
            "record": {
                "id": id, "email": email, "username": username, "name": name,
                "moul": ROOT_COLLECTION,
                "createdAt": row.get("createdAt"), "updatedAt": row.get("updatedAt"),
            }
        }),
    )
}

fn root_user(cx: &Cx<'_>, sql_where: &str, value: &str) -> ApiResult<Option<Map<String, Value>>> {
    Ok(records::query(
        cx.conn,
        &format!("SELECT * FROM writer._rootUsers WHERE {sql_where} LIMIT 1"),
        &[("v".into(), rusqlite::types::Value::Text(value.into()))],
        None,
    )?
    .into_iter()
    .next())
}

/// `POST /api/moul/:name/auth-with-password`
pub(crate) fn with_password(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let body = body(cx)?;
    let identity = str_field(&body, "identity").to_owned();
    let password = str_field(&body, "password").to_owned();
    if identity.is_empty() || password.is_empty() {
        return Err(ApiError::bad_request("identity and password are required"));
    }
    if name == ROOT_COLLECTION {
        let Some(row) = root_user(cx, "username = :v OR email = :v", &identity)? else {
            return Err(ApiError::bad_request("Invalid credentials"));
        };
        if !crypto::verify_password(&rec_str(&row, "passwordHash"), &password) {
            return Err(ApiError::bad_request("Invalid credentials"));
        }
        return Ok(root_signed_in(cx, &row));
    }
    let c = auth_collection(cx, name)?;
    let Some(rec) = find_by(cx, &c, "username = :v OR email = :v", &identity)? else {
        return Err(ApiError::bad_request("Invalid credentials"));
    };
    let Some(hash) = secret(cx.conn, &c.id, &rec_str(&rec, "id"), "passwordHash")? else {
        // An account made by an email code has no password.
        return Err(ApiError::bad_request("Invalid credentials"));
    };
    if !crypto::verify_password(&hash, &password) {
        return Err(ApiError::bad_request("Invalid credentials"));
    }
    Ok(signed_in(cx, &c, rec))
}

/// Fills the `{{.Name}}` fields of a mail template the way Go's
/// text/template does for a map: any spacing, the `{{-` and `-}}` trim
/// markers, and `<no value>` for a field the mail does not have. Other
/// actions are left as written.
pub(crate) fn render(template: &str, data: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open + 2..].find("}}").map(|c| open + 2 + c) else {
            break;
        };
        let inner = &rest[open + 2..close];
        // Go's markers need ASCII space, tab, CR or LF, and trim only those.
        const WS: [char; 4] = [' ', '\t', '\r', '\n'];
        let trim_left = inner.starts_with('-') && inner[1..].starts_with(WS);
        let trim_right = inner.ends_with('-') && inner[..inner.len() - 1].ends_with(WS);
        let mut body = inner;
        if trim_left {
            body = &body[1..];
        }
        if trim_right {
            body = &body[..body.len() - 1];
        }
        let body = body.trim_matches(WS);
        let field = body
            .strip_prefix('.')
            .filter(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        let mut before = &rest[..open];
        if trim_left {
            before = before.trim_end_matches(WS);
        }
        out += before;
        match field {
            Some(f) => {
                out += data
                    .iter()
                    .find(|(k, _)| *k == f)
                    .map_or("<no value>", |(_, v)| v)
            }
            None => out += &rest[open..close + 2],
        }
        rest = &rest[close + 2..];
        if trim_right {
            rest = rest.trim_start_matches(WS);
        }
    }
    out += rest;
    out
}

/// Keeps a mail in the local inbox. In a tab nothing is sent: Studio shows
/// the inbox, so a builder can sign in as their test users.
pub(crate) fn deliver(
    cx: &mut Cx<'_>,
    c: &Collection,
    to: &str,
    kind: &str,
    subject: &str,
    body: &str,
) -> ApiResult<()> {
    let id = ids::random_chars(cx.host, 15);
    cx.conn.execute(
        "INSERT INTO writer._inbox (id, collection, recipient, kind, subject, body, createdAt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, c.name, to, kind, subject, body, cx.now()],
    )?;
    Ok(())
}

/// `POST /api/moul/:name/request-password-reset`
pub(crate) fn request_password_reset(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = auth_collection(cx, name)?;
    let body = body(cx)?;
    let email = str_field(&body, "email").trim().to_owned();
    if email.is_empty() {
        return Err(ApiError::bad_request("email is required"));
    }
    let answer = Response::json(
        200,
        json!({"message": "If the email exists, a password reset link has been sent."}),
    );
    let Some(rec) = find_by(cx, &c, "email = :v", &email)? else {
        return Ok(answer);
    };
    let id = rec_str(&rec, "id");
    let username = rec_str(&rec, "username");
    let token = format!(
        "{}{}",
        ids::random_chars(cx.host, 15),
        ids::random_chars(cx.host, 15)
    );
    let expires = time::rfc3339(cx.now_ms + 30 * 60 * 1000);
    set_secret(
        cx.conn,
        &c.id,
        &id,
        "resetToken",
        Some(&crypto::sha256_hex(token.as_bytes())),
    )?;
    set_secret(cx.conn, &c.id, &id, "resetTokenExpiresAt", Some(&expires))?;
    let t = c.email_templates.clone().unwrap_or_default().password_reset;
    let link = format!(
        "{}/reset-password?token={token}",
        cx.config.public_url.trim_end_matches('/')
    );
    let data = [
        ("Link", link.as_str()),
        ("Token", &token),
        ("Username", &username),
        ("Email", &email),
    ];
    deliver(
        cx,
        &c,
        &email,
        "password_reset",
        &render(&t.subject, &data),
        &render(&t.body, &data),
    )?;
    Ok(answer)
}

/// `POST /api/moul/:name/confirm-password-reset`
pub(crate) fn confirm_password_reset(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = auth_collection(cx, name)?;
    let body = body(cx)?;
    let token = str_field(&body, "token").trim().to_owned();
    let password = str_field(&body, "password").to_owned();
    let confirm = str_field(&body, "passwordConfirm");
    if token.is_empty() || password.is_empty() {
        return Err(ApiError::bad_request("token and password are required"));
    }
    if !confirm.is_empty() && confirm != password {
        return Err(ApiError::bad_request(
            "password and passwordConfirm do not match",
        ));
    }
    crypto::validate_password(&password).map_err(ApiError::bad_request)?;
    // Reset tokens are stored hashed: the writer-only store never holds a
    // usable token.
    let row: Option<(String, Option<String>)> = cx
        .conn
        .query_row(
            "SELECT record, resetTokenExpiresAt FROM writer._auth WHERE collection = ?1 AND resetToken = ?2",
            params![c.id, crypto::sha256_hex(token.as_bytes())],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((record, expires)) = row else {
        return Err(ApiError::bad_request(
            "Invalid or expired password reset token",
        ));
    };
    let Some(expires) = expires.filter(|e| !e.is_empty()) else {
        return Err(ApiError::bad_request(
            "Invalid or expired password reset token",
        ));
    };
    if time::parse_rfc3339(&expires).is_none_or(|e| cx.now_ms > e) {
        return Err(ApiError::bad_request("Password reset token has expired"));
    }
    let hash = crypto::hash_password(cx.host, &password);
    set_secret(cx.conn, &c.id, &record, "passwordHash", Some(&hash))?;
    set_secret(cx.conn, &c.id, &record, "resetToken", None)?;
    set_secret(cx.conn, &c.id, &record, "resetTokenExpiresAt", None)?;
    Ok(Response::json(
        200,
        json!({"message": "Password reset successfully"}),
    ))
}

/// `POST /api/moul/:name/refresh`: exchanges a token, even an expired one
/// within the grace period, for a new one. The old token is revoked, so it
/// is exchanged once.
pub(crate) fn refresh(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let mut token = bearer(cx.req).map(str::to_owned).unwrap_or_default();
    if token.is_empty()
        && let Ok(Some(b)) = cx.req.json_object()
    {
        token = str_field(&b, "token").trim().to_owned();
    }
    if token.is_empty() {
        return Err(ApiError::bad_request("Authorization token is required"));
    }
    if is_revoked(cx.conn, &token, cx.now_ms) {
        return Err(ApiError::unauthorized("Token has been revoked"));
    }
    let claims = crypto::verify_token(cx.key, &token, None)
        .map_err(|e| ApiError::unauthorized(format!("Invalid token: {e}")))?;
    if claims.collection != name {
        return Err(ApiError::unauthorized(
            "Token does not belong to this collection",
        ));
    }
    let Some(exp) = claims.exp else {
        return Err(ApiError::unauthorized("Invalid token: missing expiry"));
    };
    let exp_ms = exp * 1000;
    if cx.now_ms - exp_ms > cx.config.refresh_grace_ms {
        return Err(ApiError::unauthorized(
            "Token expired too long ago; sign in again",
        ));
    }
    revoke(
        cx.conn,
        &token,
        exp_ms + cx.config.refresh_grace_ms,
        cx.now_ms,
    )?;
    if name == ROOT_COLLECTION {
        let Some(row) = root_user(cx, "id = :v", &claims.id)? else {
            return Err(ApiError::unauthorized("User record not found"));
        };
        return Ok(root_signed_in(cx, &row));
    }
    let c = auth_collection(cx, name)?;
    let Some(rec) = records::load(cx.conn, &c, &claims.id)? else {
        return Err(ApiError::unauthorized("User record not found"));
    };
    Ok(signed_in(cx, &c, rec))
}

/// `POST /api/moul/:name/logout`: revokes the token for as long as it
/// could still be refreshed.
pub(crate) fn logout(cx: &mut Cx<'_>) -> ApiResult {
    let h = cx.req.header_value("Authorization");
    if h.is_empty() {
        return Err(ApiError::bad_request("Authorization header is required"));
    }
    let parts: Vec<&str> = h.split(' ').collect();
    if parts.len() != 2 || !parts[0].eq_ignore_ascii_case("bearer") {
        return Err(ApiError::bad_request("Invalid Authorization header format"));
    }
    let token = parts[1];
    // A token this engine did not sign opens nothing, so there is nothing
    // to revoke; storing it would let anyone grow the table.
    if let Ok(claims) = crypto::verify_token(cx.key, token, None) {
        let exp = claims
            .exp
            .map(|e| e * 1000)
            .unwrap_or(cx.now_ms + cx.config.token_ttl_ms);
        revoke(cx.conn, token, exp + cx.config.refresh_grace_ms, cx.now_ms)?;
    }
    Ok(Response::json(
        200,
        json!({"message": "Logged out successfully"}),
    ))
}

/// Limits on guessing email codes, kept in memory like the Go engine's: 5
/// guesses per code, 20 failures a day per account, 30 failures per 10
/// minutes per network.
#[derive(Debug, Default)]
pub struct OtpThrottle {
    accounts: HashMap<String, (i64, u32, u32)>,
    networks: HashMap<String, (i64, u32)>,
}

const OTP_CODE_ATTEMPTS: u32 = 5;
const OTP_DAILY_FAILURES: u32 = 20;
const OTP_NETWORK_FAILURES: u32 = 30;
const OTP_NETWORK_WINDOW_MS: i64 = 10 * 60 * 1000;
const DAY_MS: i64 = 24 * 3600 * 1000;

impl OtpThrottle {
    fn account(&mut self, key: &str, now: i64) -> &mut (i64, u32, u32) {
        let s = self.accounts.entry(key.to_owned()).or_insert((now, 0, 0));
        if now - s.0 >= DAY_MS {
            s.0 = now;
            s.2 = 0;
        }
        s
    }

    fn network(&mut self, key: &str, now: i64) -> &mut (i64, u32) {
        let s = self.networks.entry(key.to_owned()).or_insert((now, 0));
        if now - s.0 >= OTP_NETWORK_WINDOW_MS {
            *s = (now, 0);
        }
        s
    }

    /// Forgets idle state once many keys are tracked, as the Go engine does.
    fn prune(&mut self, now: i64) {
        if self.accounts.len() + self.networks.len() < 10_000 {
            return;
        }
        self.accounts.retain(|_, s| now - s.0 < DAY_MS);
        self.networks
            .retain(|_, s| now - s.0 < OTP_NETWORK_WINDOW_MS);
    }

    /// Takes one attempt from every budget; on refusal, names the spent one
    /// and when to retry, in seconds.
    fn reserve(
        &mut self,
        account: &str,
        network: &str,
        now: i64,
    ) -> Result<(), (&'static str, i64)> {
        self.prune(now);
        let (day_start, code, day) = *self.account(account, now);
        let (win_start, net) = *self.network(network, now);
        if day >= OTP_DAILY_FAILURES {
            return Err(("account", (DAY_MS - (now - day_start)) / 1000));
        }
        if code >= OTP_CODE_ATTEMPTS {
            return Err(("code", 60));
        }
        if net >= OTP_NETWORK_FAILURES {
            return Err((
                "network",
                (OTP_NETWORK_WINDOW_MS - (now - win_start)) / 1000,
            ));
        }
        let a = self.account(account, now);
        a.1 += 1;
        a.2 += 1;
        self.network(network, now).1 += 1;
        Ok(())
    }

    fn code_exhausted(&mut self, account: &str, now: i64) -> bool {
        self.account(account, now).1 >= OTP_CODE_ATTEMPTS
    }

    fn new_code(&mut self, account: &str, now: i64) {
        self.account(account, now).1 = 0;
    }

    fn succeeded(&mut self, account: &str, network: &str) {
        self.accounts.remove(account);
        if let Some(n) = self.networks.get_mut(network) {
            n.1 = n.1.saturating_sub(1);
        }
    }
}

/// The network an email-code guess comes from: the address the host saw,
/// IPv4 as is and IPv6 by its /64, so one client cannot spread guesses over
/// a block. Without one (a tab has a single user) every guess shares one
/// budget.
fn network_key(cx: &Cx<'_>) -> String {
    let Some(addr) = cx.req.remote.as_deref() else {
        return String::new();
    };
    let ip = addr.trim().trim_start_matches('[');
    let ip = ip.split(']').next().unwrap_or(ip);
    if let Ok(v6) = ip.parse::<std::net::Ipv6Addr>() {
        if let Some(v4) = v6.to_ipv4_mapped() {
            return v4.to_string();
        }
        let s = v6.segments();
        return format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3]);
    }
    // An IPv4 address, possibly with a port.
    ip.split(':').next().unwrap_or(ip).to_owned()
}

/// `POST /api/moul/:name/otp/request`: makes a 6-digit code and puts it in
/// the inbox. An unknown email signs up: a record is created for it.
pub(crate) fn request_otp(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = auth_collection(cx, name)?;
    let body = body(cx)?;
    let email = str_field(&body, "email").trim().to_owned();
    if email.is_empty() {
        return Err(ApiError::bad_request("email is required"));
    }
    let (id, username) = match find_by(cx, &c, "email = :v", &email)? {
        Some(rec) => (rec_str(&rec, "id"), rec_str(&rec, "username")),
        None => {
            let base: String = email
                .split('@')
                .next()
                .unwrap_or("")
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            let base = if base.is_empty() {
                "user".to_owned()
            } else {
                base
            };
            let mut username = base.clone();
            while find_by(cx, &c, "username = :v", &username)?.is_some() {
                username = format!("{base}_{}", ids::random_chars(cx.host, 4));
            }
            let id = ids::record_id(cx.host, &c.name);
            let now = cx.now();
            cx.conn.execute(
                &format!(
                    "INSERT INTO {} (id, username, email, createdAt, updatedAt) VALUES (?1, ?2, ?3, ?4, ?4)",
                    quote(&c.name)
                ),
                params![id, username, email, now],
            )?;
            if let Some(mut rec) = records::load(cx.conn, &c, &id)? {
                values::normalize(&c, &mut rec);
                cx.events.push(RecordEvent {
                    action: Action::Create,
                    collection: c.clone(),
                    record: rec,
                    old: None,
                    from_log: false,
                });
            }
            cx.changed(&c, Some(&id), vec!["username".into(), "email".into()]);
            (id, username)
        }
    };
    let mut digits = [0u8; 6];
    cx.host.fill_random(&mut digits);
    let code: String = digits.iter().map(|b| char::from(b'0' + b % 10)).collect();
    let expires = time::rfc3339(cx.now_ms + 10 * 60 * 1000);
    set_secret(cx.conn, &c.id, &id, "otpCode", Some(&code))?;
    set_secret(cx.conn, &c.id, &id, "otpExpiresAt", Some(&expires))?;
    cx.otp.new_code(&format!("{}|{email}", c.name), cx.now_ms);
    let t = c.email_templates.clone().unwrap_or_default().otp;
    let data = [
        ("OTP", code.as_str()),
        ("Username", &username),
        ("Email", &email),
    ];
    deliver(
        cx,
        &c,
        &email,
        "otp",
        &render(&t.subject, &data),
        &render(&t.body, &data),
    )?;
    Ok(Response::json(
        200,
        json!({"message": "OTP generated and sent successfully"}),
    ))
}

/// `POST /api/moul/:name/auth-with-otp`
pub(crate) fn with_otp(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = auth_collection(cx, name)?;
    let body = body(cx)?;
    let email = str_field(&body, "email").trim().to_owned();
    let code = str_field(&body, "code").trim().to_owned();
    if email.is_empty() || code.is_empty() {
        return Err(ApiError::bad_request("email and code are required"));
    }
    let account = format!("{}|{email}", c.name);
    let network = network_key(cx);
    if let Err((reason, retry)) = cx.otp.reserve(&account, &network, cx.now_ms) {
        return Ok(Response::json(
            429,
            json!({
                "message": "Too many sign-in attempts. Request a new code or try again later.",
                "reason": reason,
            }),
        )
        .with_header("Retry-After", &(retry + 1).to_string()));
    }
    let Some(rec) = find_by(cx, &c, "email = :v", &email)? else {
        return Err(ApiError::bad_request("Invalid email or OTP code"));
    };
    let id = rec_str(&rec, "id");
    let stored = secret(cx.conn, &c.id, &id, "otpCode")?.unwrap_or_default();
    if stored.is_empty() {
        return Err(ApiError::bad_request(
            "No active OTP found for this email. Please request a new one.",
        ));
    }
    if !crypto::constant_time_eq(stored.as_bytes(), code.as_bytes()) {
        if cx.otp.code_exhausted(&account, cx.now_ms) {
            // The code used up its guesses: void it so only a new code works.
            set_secret(cx.conn, &c.id, &id, "otpCode", None)?;
            set_secret(cx.conn, &c.id, &id, "otpExpiresAt", None)?;
        }
        return Err(ApiError::bad_request("Invalid email or OTP code"));
    }
    let expires = secret(cx.conn, &c.id, &id, "otpExpiresAt")?.unwrap_or_default();
    let Some(expires) = time::parse_rfc3339(&expires) else {
        return Err(ApiError::bad_request("Invalid email or OTP code"));
    };
    if cx.now_ms > expires {
        return Err(ApiError::bad_request(
            "OTP code has expired. Please request a new one.",
        ));
    }
    cx.otp.succeeded(&account, &network);
    set_secret(cx.conn, &c.id, &id, "otpCode", None)?;
    set_secret(cx.conn, &c.id, &id, "otpExpiresAt", None)?;
    Ok(signed_in(cx, &c, rec))
}

/// The username a new account starts from: `base` with only letters,
/// digits and `_` kept (`user` if nothing is left), and a suffix if that is
/// taken.
fn free_username(cx: &mut Cx<'_>, c: &Collection, base: &str) -> ApiResult<String> {
    let base: String = base
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    let base = if base.is_empty() {
        "user".to_owned()
    } else {
        base
    };
    let mut username = base.clone();
    while find_by(cx, c, "username = :v", &username)?.is_some() {
        username = format!("{base}_{}", ids::random_chars(cx.host, 4));
    }
    Ok(username)
}

/// Signs in the person a provider vouched for: the account already linked
/// to their id at the provider, else the account with their email if the
/// provider says the email is theirs (and links it), else a new account.
/// The host has checked the provider's answer; see [`crate::oauth`].
pub(crate) fn external(
    cx: &mut Cx<'_>,
    name: &str,
    profile: &crate::oauth::ExternalProfile,
) -> ApiResult {
    let c = auth_collection(cx, name)?;
    if profile.provider.is_empty() || profile.id.is_empty() {
        return Err(ApiError::bad_request(
            "The provider did not identify the account",
        ));
    }
    // An account already linked to this id.
    let linked: Vec<(String, String)> = {
        let mut stmt = cx.conn.prepare(
            "SELECT record, oauthProviders FROM writer._auth WHERE collection = ?1 AND oauthProviders IS NOT NULL",
        )?;
        stmt.query_map([&c.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let matches = |list: &str| -> bool {
        serde_json::from_str::<Vec<Value>>(list).is_ok_and(|links| {
            links.iter().any(|l| {
                l["provider"]
                    .as_str()
                    .is_some_and(|p| p.eq_ignore_ascii_case(&profile.provider))
                    && l["id"].as_str() == Some(profile.id.as_str())
            })
        })
    };
    if let Some((record, _)) = linked.iter().find(|(_, list)| matches(list))
        && let Some(rec) = records::load(cx.conn, &c, record)?
    {
        return Ok(signed_in(cx, &c, rec));
    }
    let add_link = |cx: &mut Cx<'_>, record: &str| -> ApiResult<()> {
        let mut links: Vec<Value> = secret(cx.conn, &c.id, record, "oauthProviders")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        links.push(profile.link());
        set_secret(
            cx.conn,
            &c.id,
            record,
            "oauthProviders",
            Some(&Value::Array(links).to_string()),
        )
    };
    let email = profile.email.trim();
    if !email.is_empty()
        && let Some(rec) = find_by(cx, &c, "email = :v", email)?
    {
        if !profile.email_verified {
            return Err(ApiError::bad_request(format!(
                "An account with this email exists. {} has not confirmed the address is yours, so sign in to the account another way.",
                profile.provider
            )));
        }
        // An account with a password may have been made by someone who
        // only typed this address: linking would let them keep signing in
        // with their password. The owner of the address resets the
        // password by mail instead, and owns the account from then on.
        let id = rec_str(&rec, "id");
        if secret(cx.conn, &c.id, &id, "passwordHash")?.is_some() {
            return Err(ApiError::bad_request(format!(
                "An account with this email has a password. Sign in with it, or reset it by email, rather than with {}.",
                profile.provider
            )));
        }
        add_link(cx, &id)?;
        return Ok(signed_in(cx, &c, rec));
    }
    // A new account.
    let base = [
        &profile.username,
        &email.split('@').next().unwrap_or("").to_owned(),
        &profile.name,
    ]
    .into_iter()
    .find(|s| !s.trim().is_empty())
    .cloned()
    .unwrap_or_default();
    let username = free_username(cx, &c, &base)?;
    // An address the provider does not vouch for is not recorded as the
    // account's: a later sign-in that does vouch for it must not land here.
    let email = if email.is_empty() || !profile.email_verified {
        format!("{}@{}.oauth", profile.id, profile.provider)
    } else {
        email.to_owned()
    };
    let id = ids::record_id(cx.host, &c.name);
    let now = cx.now();
    cx.conn.execute(
        &format!(
            "INSERT INTO {} (id, username, email, createdAt, updatedAt) VALUES (?1, ?2, ?3, ?4, ?4)",
            quote(&c.name)
        ),
        params![id, username, email, now],
    )?;
    add_link(cx, &id)?;
    let rec = records::load(cx.conn, &c, &id)?.ok_or_else(ApiError::internal)?;
    let mut shown = rec.clone();
    values::normalize(&c, &mut shown);
    cx.events.push(RecordEvent {
        action: Action::Create,
        collection: c.clone(),
        record: shown,
        old: None,
        from_log: false,
    });
    cx.changed(&c, Some(&id), vec!["username".into(), "email".into()]);
    Ok(signed_in(cx, &c, rec))
}

/// `POST /api/moul/:name/email-templates/test` with `{"email", "template"}`:
/// renders the template with sample values and puts the mail in the inbox,
/// from which a native server sends it.
pub(crate) fn test_template(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    cx.require_admin()?;
    let c = auth_collection(cx, name)?;
    let body = body(cx)?;
    let email = str_field(&body, "email").trim().to_owned();
    let key = str_field(&body, "template").trim().to_owned();
    if email.is_empty() {
        return Err(ApiError::bad_request("email is required"));
    }
    if key.is_empty() {
        return Err(ApiError::bad_request("template key is required"));
    }
    let t = c.email_templates.clone().unwrap_or_default();
    let template = match key.as_str() {
        "verification" => t.verification,
        "password_reset" => t.password_reset,
        "confirm_email_change" => t.confirm_email_change,
        "otp" => t.otp,
        "login_alert" => t.login_alert,
        _ => {
            return Err(ApiError::bad_request(
                "invalid template key: must be verification, password_reset, confirm_email_change, otp, or login_alert",
            ));
        }
    };
    let link = format!(
        "{}/dummy-link-test",
        cx.config.public_url.trim_end_matches('/')
    );
    let data = [
        ("Link", link.as_str()),
        ("OTP", "123456"),
        ("Username", "TestUser"),
        ("Email", email.as_str()),
    ];
    deliver(
        cx,
        &c,
        &email,
        "test",
        &render(&template.subject, &data),
        &render(&template.body, &data),
    )?;
    Ok(Response::json(
        200,
        json!({"message": "Test email sent/queued successfully"}),
    ))
}

/// `GET /api/inbox?to=<email>&collection=<name>`: the local inbox, newest
/// first, owner only.
pub(crate) fn inbox_endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let to = cx.req.query_param("to").to_owned();
    let collection = cx.req.query_param("collection").to_owned();
    let mut stmt = cx.conn.prepare(
        "SELECT id, collection, recipient, kind, subject, body, createdAt FROM writer._inbox
         WHERE (?1 = '' OR recipient = ?1) AND (?2 = '' OR collection = ?2)
         ORDER BY createdAt DESC, rowid DESC LIMIT 200",
    )?;
    let items = stmt
        .query_map(params![to, collection], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "collection": r.get::<_, String>(1)?,
                "to": r.get::<_, String>(2)?,
                "kind": r.get::<_, String>(3)?,
                "subject": r.get::<_, String>(4)?,
                "body": r.get::<_, String>(5)?,
                "createdAt": r.get::<_, String>(6)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Response::json(200, json!({"items": items})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_render_like_go() {
        let data = [("OTP", "123456"), ("Email", "a@b.c")];
        assert_eq!(render("Code: {{.OTP}}", &data), "Code: 123456");
        assert_eq!(render("Code: {{ .OTP }}!", &data), "Code: 123456!");
        assert_eq!(render("Code:  {{- .OTP -}}  !", &data), "Code:123456!");
        assert_eq!(render("{{.Link}}", &data), "<no value>");
        assert_eq!(render("{{if .OTP}}x{{end}}", &data), "{{if .OTP}}x{{end}}");
        assert_eq!(
            render("open {{ and no close", &data),
            "open {{ and no close"
        );
        // Only ASCII whitespace is trimmed, as in Go.
        assert_eq!(
            render("Code:\u{3000}{{- .OTP}}", &data),
            "Code:\u{3000}123456"
        );
    }
}
