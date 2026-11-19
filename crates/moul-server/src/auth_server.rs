//! An OAuth 2.1 authorization server for MCP clients, and the device flow,
//! as the Go engine offers them. A client registers itself, sends a root
//! user to a consent page to sign in, and gets a root user's token for
//! `/api/mcp`.
//!
//! | Path | |
//! | --- | --- |
//! | `GET /.well-known/oauth-protected-resource[/*]` | which server issues tokens for `/api/mcp` |
//! | `GET /.well-known/oauth-authorization-server`, `/.well-known/openid-configuration` | the endpoints below |
//! | `POST /api/oauth2/register` | dynamic client registration (RFC 7591) |
//! | `GET`, `POST /api/oauth2/authorize` | the consent page, where a root user signs in |
//! | `POST /api/oauth2/token` | a code or a refresh token for an access token |
//! | `POST /api/oauth2/device/authorize`, `/api/oauth2/device/token` | the device flow (RFC 8628), for terminals |
//! | `GET /device`, `POST /device/verify` | where a root user approves a device |
//!
//! Where this departs from Go, which trusted anything:
//!
//! - A client must have registered, and the `redirect_uri` must be one it
//!   registered. Go registered any `client_id` it was shown and added any
//!   `redirect_uri` to it, so codes could be sent anywhere.
//! - Only a root user's password signs in on the consent page. Go also
//!   took the admin key typed as either the user name or the password.
//! - PKCE is S256, required for public clients; a confidential client must
//!   present its secret.
//! - A refresh token is used once and replaced; Go handed back the same
//!   one forever.
//! - A device code's token is handed out once; `slow_down` answers polling
//!   faster than the interval.
//! - Clients and refresh tokens are kept in the writer-only store, so a
//!   restart does not sign MCP clients out. Codes live in memory.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use moul_core::crypto;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::http::{AppState, ClientIp, base_url, engine_headers, error, peer_trusted};

/// How long an authorization code lasts.
const CODE_TTL_MS: i64 = 5 * 60 * 1000;
/// How long a refresh token lasts.
const REFRESH_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
/// How long a device code lasts.
const DEVICE_TTL_MS: i64 = 5 * 60 * 1000;
/// How often a device may poll, in seconds.
const DEVICE_INTERVAL: i64 = 5;

struct Code {
    client: String,
    redirect: String,
    challenge: String,
    scope: String,
    root: String,
    token: String,
    expires: i64,
}

struct Device {
    user_code: String,
    expires: i64,
    last_poll: i64,
    token: Option<String>,
}

/// Codes not yet traded.
#[derive(Default)]
pub struct Store {
    codes: Mutex<HashMap<String, Code>>,
    devices: Mutex<HashMap<String, Device>>,
}

fn now() -> i64 {
    moul_vfs::system_clock()
}

fn random(n: usize) -> String {
    moul_core::ids::random_chars(&mut crate::open::OsRandom, n)
}

/// The routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/{*rest}",
            get(protected_resource_at),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server/{*rest}",
            get(server_metadata),
        )
        .route("/.well-known/openid-configuration", get(server_metadata))
        .route("/api/oauth2/register", post(register))
        .route(
            "/api/oauth2/authorize",
            get(authorize_page).post(authorize_submit),
        )
        .route("/api/oauth2/token", post(token))
        .route("/api/oauth2/device/authorize", post(device_authorize))
        .route("/api/oauth2/device/token", post(device_token))
        .route("/device", get(device_page))
        .route("/device/verify", post(device_verify))
}

fn base(state: &AppState, req: &Request) -> String {
    base_url(state, req.headers(), peer_trusted(state, req))
}

async fn protected_resource(State(state): State<AppState>, req: Request) -> Response {
    let b = base(&state, &req);
    resource_doc(&state, &b, &format!("{b}/api/mcp"))
}

async fn protected_resource_at(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    req: Request,
) -> Response {
    let b = base(&state, &req);
    resource_doc(&state, &b, &format!("{b}/{}", rest.trim_start_matches('/')))
}

fn resource_doc(state: &AppState, b: &str, resource: &str) -> Response {
    axum::Json(json!({
        "resource": resource,
        "authorization_servers": [b],
        "scopes_supported": ["mcp", "admin"],
        "bearer_methods_supported": ["header"],
        "resource_name": format!("{} MCP Server", state.app_name()),
        "resource_documentation": format!("{b}/docs"),
    }))
    .into_response()
}

async fn server_metadata(State(state): State<AppState>, req: Request) -> Response {
    let b = base(&state, &req);
    axum::Json(json!({
        "issuer": b,
        "authorization_endpoint": format!("{b}/api/oauth2/authorize"),
        "token_endpoint": format!("{b}/api/oauth2/token"),
        "registration_endpoint": format!("{b}/api/oauth2/register"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic", "none"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": ["mcp", "admin"],
    }))
    .into_response()
}

fn oauth_error(status: u16, code: &str, description: &str) -> Response {
    let mut r =
        axum::Json(json!({"error": code, "error_description": description})).into_response();
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    r
}

async fn body_json(req: Request) -> (HeaderMap, Value) {
    let headers = req.headers().clone();
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let v = if ct.starts_with("application/x-www-form-urlencoded") {
        Value::Object(
            moul_core::api::parse_query(&String::from_utf8_lossy(&bytes))
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        )
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (headers, v)
}

fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").trim().to_owned()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `POST /api/oauth2/register`
async fn register(State(state): State<AppState>, req: Request) -> Response {
    let (_, b) = body_json(req).await;
    let redirects = strings(&b["redirect_uris"]);
    if redirects.is_empty() {
        return oauth_error(
            400,
            "invalid_redirect_uri",
            "redirect_uris must name at least one address",
        );
    }
    if redirects.len() > 10 || redirects.iter().any(|r| r.len() > 2048) {
        return oauth_error(
            400,
            "invalid_redirect_uri",
            "at most 10 redirect_uris of at most 2048 characters",
        );
    }
    if text(&b, "client_name").chars().count() > 200 {
        return oauth_error(
            400,
            "invalid_client_metadata",
            "client_name is at most 200 characters",
        );
    }
    for r in &redirects {
        let ok = reqwest::Url::parse(r).is_ok_and(|u| {
            u.fragment().is_none()
                && (u.scheme() == "https"
                    || (u.scheme() == "http"
                        && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
                    || !matches!(u.scheme(), "http" | "javascript" | "data" | "file"))
        });
        if !ok {
            return oauth_error(
                400,
                "invalid_redirect_uri",
                &format!("{r} cannot receive codes"),
            );
        }
    }
    let name = Some(text(&b, "client_name"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Custom MCP Client".into());
    let method = Some(text(&b, "token_endpoint_auth_method"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "client_secret_post".into());
    if !["client_secret_post", "client_secret_basic", "none"].contains(&method.as_str()) {
        return oauth_error(
            400,
            "invalid_client_metadata",
            "token_endpoint_auth_method is not supported",
        );
    }
    let mut grants = strings(&b["grant_types"]);
    grants.retain(|g| g == "authorization_code" || g == "refresh_token");
    if grants.is_empty() {
        grants = vec!["authorization_code".into(), "refresh_token".into()];
    }
    let responses = vec!["code".to_owned()];
    let id = format!("moul_client_{}", random(16));
    let secret = (method != "none").then(|| format!("moul_sec_{}", random(32)));
    let issued = now() / 1000;
    let row = (
        id.clone(),
        secret.as_deref().map(|s| crypto::sha256_hex(s.as_bytes())),
        name.clone(),
        json!(redirects).to_string(),
        json!(grants).to_string(),
        json!(responses).to_string(),
        method.clone(),
        moul_core::time::rfc3339(now()),
    );
    let saved = state
        .db
        .run(move |db| {
            // Clients that never finished signing in are let go after a day.
            let _ = db.engine.store().conn().execute(
                "DELETE FROM writer._oauth_clients WHERE createdAt < ?1
                 AND id NOT IN (SELECT client FROM writer._oauth_refresh)",
                [moul_core::time::rfc3339(now() - 24 * 3600 * 1000)],
            );
            db.engine.store().conn().execute(
                "INSERT INTO writer._oauth_clients (id, secretHash, name, redirectUris, grantTypes, responseTypes, authMethod, createdAt)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7],
            )
        })
        .await;
    if !matches!(saved, Ok(Ok(_))) {
        return oauth_error(500, "server_error", "the client could not be saved");
    }
    let mut out = json!({
        "client_id": id,
        "client_name": name,
        "redirect_uris": redirects,
        "grant_types": grants,
        "response_types": responses,
        "token_endpoint_auth_method": method,
        "client_id_issued_at": issued,
    });
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
        out["client_secret_expires_at"] = json!(0);
    }
    let mut r = axum::Json(out).into_response();
    *r.status_mut() = StatusCode::CREATED;
    r
}

struct Client {
    name: String,
    secret_hash: Option<String>,
    redirects: Vec<String>,
    method: String,
}

async fn client(state: &AppState, id: &str) -> Option<Client> {
    let id = id.to_owned();
    state
        .db
        .run(move |db| {
            db.engine
                .store()
                .conn()
                .query_row(
                    "SELECT name, secretHash, redirectUris, authMethod FROM writer._oauth_clients WHERE id = ?1",
                    [id],
                    |r| {
                        Ok(Client {
                            name: r.get(0)?,
                            secret_hash: r.get(1)?,
                            redirects: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                            method: r.get(3)?,
                        })
                    },
                )
                .optional()
                .ok()
                .flatten()
        })
        .await
        .ok()
        .flatten()
}

fn same_uri(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Escapes text for HTML.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn page(status: u16, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t}</title><style>
:root{{color-scheme:light dark;--bg:#f7f7f5;--fg:#1d1d1b;--muted:#6b6b66;--card:#fff;--line:#e2e2dd;--accent:#2b59c3;--bad:#b42318}}
@media (prefers-color-scheme:dark){{:root{{--bg:#151514;--fg:#ededea;--muted:#a3a39d;--card:#1f1f1d;--line:#34342f;--accent:#7c9cf0;--bad:#f97066}}}}
body{{margin:0;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;padding:16px;box-sizing:border-box}}
main{{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:28px;max-width:420px;width:100%;box-sizing:border-box}}
h1{{font-size:20px;margin:0 0 8px}}p{{color:var(--muted);margin:0 0 16px}}label{{display:block;font-size:14px;margin:12px 0 4px}}
input{{width:100%;box-sizing:border-box;padding:10px;border:1px solid var(--line);border-radius:8px;background:var(--bg);color:var(--fg);font:inherit}}
button{{margin-top:18px;width:100%;padding:11px;border:0;border-radius:8px;background:var(--accent);color:#fff;font:inherit;font-weight:600;cursor:pointer}}
.err{{color:var(--bad);margin:0 0 12px}}code{{font-size:14px}}a{{color:var(--accent)}}
</style></head><body><main>{body}</main></body></html>"#,
        t = esc(title)
    );
    let mut r = Response::new(Body::from(html));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    r
}

struct Ask {
    client: String,
    redirect: String,
    state: String,
    challenge: String,
    method: String,
    scope: String,
}

fn ask_of(v: &Value) -> Ask {
    Ask {
        client: text(v, "client_id"),
        redirect: text(v, "redirect_uri"),
        state: v["state"].as_str().unwrap_or("").to_owned(),
        challenge: text(v, "code_challenge"),
        method: text(v, "code_challenge_method"),
        scope: Some(text(v, "scope"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "mcp".into()),
    }
}

/// Checks a request for consent; the client's name if it holds.
async fn check_ask(state: &AppState, a: &Ask, response_type: &str) -> Result<Client, Response> {
    if a.client.is_empty() || a.redirect.is_empty() {
        return Err(error(400, "client_id and redirect_uri are required"));
    }
    if !response_type.is_empty() && response_type != "code" {
        return Err(error(
            400,
            &format!("unsupported_response_type: {response_type}"),
        ));
    }
    let Some(c) = client(state, &a.client).await else {
        return Err(error(400, "Unknown client_id: register the client first"));
    };
    if !c.redirects.iter().any(|r| same_uri(r, &a.redirect)) {
        return Err(error(400, "redirect_uri is not one this client registered"));
    }
    if c.method == "none" && a.challenge.is_empty() {
        return Err(error(400, "code_challenge is required (PKCE, S256)"));
    }
    if !a.challenge.is_empty() && !a.method.is_empty() && a.method != "S256" {
        return Err(error(400, "code_challenge_method must be S256"));
    }
    Ok(c)
}

fn consent(state: &AppState, c: &Client, a: &Ask, err: Option<&str>) -> Response {
    let host = reqwest::Url::parse(&a.redirect)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    let hidden = [
        ("client_id", &a.client),
        ("redirect_uri", &a.redirect),
        ("response_type", &"code".to_owned()),
        ("scope", &a.scope),
        ("state", &a.state),
        ("code_challenge", &a.challenge),
        ("code_challenge_method", &a.method),
    ]
    .iter()
    .map(|(k, v)| format!(r#"<input type="hidden" name="{k}" value="{}">"#, esc(v)))
    .collect::<String>();
    let cancel = format!(
        "{}{}error=access_denied&state={}",
        a.redirect,
        if a.redirect.contains('?') { "&" } else { "?" },
        moul_core::oauth::encode(&a.state)
    );
    let body = format!(
        r#"<h1>Allow {client} into {app}?</h1>
<p>It will act as the root user who signs in here, with full access to the data. Its answers go to <code>{host}</code>.</p>
{err}<form method="post">{hidden}
<label for="identity">Root user name or email</label><input id="identity" name="identity" autocomplete="username" required autofocus>
<label for="password">Password</label><input id="password" name="password" type="password" autocomplete="current-password" required>
<button type="submit">Sign in and allow</button></form>
<p style="margin-top:14px"><a href="{cancel}">Cancel</a></p>"#,
        client = esc(&c.name),
        app = esc(&state.app_name()),
        host = esc(&host),
        err = err
            .map(|e| format!(r#"<p class="err">{}</p>"#, esc(e)))
            .unwrap_or_default(),
        cancel = esc(&cancel),
    );
    page(
        if err.is_some() { 400 } else { 200 },
        &format!("Authorize {} - {} MCP", c.name, state.app_name()),
        &body,
    )
}

/// `GET /api/oauth2/authorize`
async fn authorize_page(State(state): State<AppState>, req: Request) -> Response {
    let q: Value = Value::Object(
        moul_core::api::parse_query(req.uri().query().unwrap_or(""))
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
    );
    let a = ask_of(&q);
    match check_ask(&state, &a, &text(&q, "response_type")).await {
        Ok(c) => consent(&state, &c, &a, None),
        Err(r) => r,
    }
}

/// Signs a root user in with a password: their token, or why not.
async fn root_sign_in(
    state: &AppState,
    identity: &str,
    password: &str,
    ip: Option<String>,
) -> Result<(String, String), String> {
    let body = json!({"identity": identity, "password": password});
    let r = state
        .db
        .run(move |db| {
            let mut req = moul_core::Request::post("/api/moul/_rootUsers/auth-with-password", body);
            req.remote = ip.clone();
            let r = db.engine.handle(req);
            if r.status != 200 {
                return Err(r.message().to_owned());
            }
            let token = r.json_body()["token"].as_str().unwrap_or("").to_owned();
            let id = r.json_body()["record"]["id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            // A root user held to a list of addresses signs in only from one.
            let creds = vec![("Authorization".to_owned(), format!("Bearer {token}"))];
            match db.engine.identity(&creds, ip.as_deref()) {
                Ok(_) => Ok((token, id)),
                Err(r) => Err(r.message().to_owned()),
            }
        })
        .await
        .map_err(|e| e.to_string())?;
    r.map_err(|m| {
        if m.is_empty() {
            "Invalid credentials".into()
        } else {
            m
        }
    })
}

/// `POST /api/oauth2/authorize`: the consent form.
async fn authorize_submit(State(state): State<AppState>, req: Request) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0.to_string());
    let (_, form) = body_json(req).await;
    let a = ask_of(&form);
    let c = match check_ask(&state, &a, &text(&form, "response_type")).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (identity, password) = (
        text(&form, "identity"),
        form["password"].as_str().unwrap_or("").to_owned(),
    );
    if identity.is_empty() || password.is_empty() {
        return consent(
            &state,
            &c,
            &a,
            Some("Enter the root user's name or email and password."),
        );
    }
    let (token, root) = match root_sign_in(&state, &identity, &password, ip).await {
        Ok(t) => t,
        Err(_) => {
            return consent(
                &state,
                &c,
                &a,
                Some("Those are not a root user's credentials."),
            );
        }
    };
    let code = random(32);
    {
        let mut codes = state.auth.codes.lock().unwrap_or_else(|e| e.into_inner());
        let t = now();
        codes.retain(|_, v| v.expires > t);
        codes.insert(
            code.clone(),
            Code {
                client: a.client.clone(),
                redirect: a.redirect.clone(),
                challenge: a.challenge.clone(),
                scope: a.scope.clone(),
                root,
                token,
                expires: t + CODE_TTL_MS,
            },
        );
    }
    let mut to = a.redirect.clone();
    to.push(if to.contains('?') { '&' } else { '?' });
    to.push_str(&format!("code={code}"));
    if !a.state.is_empty() {
        to.push_str(&format!("&state={}", moul_core::oauth::encode(&a.state)));
    }
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::FOUND;
    if let Ok(v) = HeaderValue::try_from(to) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}

fn no_store(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    r
}

async fn issue_refresh(state: &AppState, client: &str, root: &str, scope: &str) -> Option<String> {
    let token = random(40);
    let row = (
        crypto::sha256_hex(token.as_bytes()),
        client.to_owned(),
        root.to_owned(),
        scope.to_owned(),
        moul_core::time::rfc3339(now() + REFRESH_TTL_MS),
    );
    let ok = state
        .db
        .run(move |db| {
            let conn = db.engine.store().conn();
            let _ = conn.execute(
                "DELETE FROM writer._oauth_refresh WHERE expiresAt < ?1",
                [moul_core::time::rfc3339(now())],
            );
            conn.execute(
                "INSERT INTO writer._oauth_refresh (tokenHash, client, rootUser, scope, expiresAt) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![row.0, row.1, row.2, row.3, row.4],
            )
            .is_ok()
        })
        .await
        .unwrap_or(false);
    ok.then_some(token)
}

fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let h = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = h.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(crypto::base64_decode(rest.trim())?).ok()?;
    let (id, secret) = decoded.split_once(':')?;
    Some((
        moul_core::api::parse_query(&format!("x={id}")).pop()?.1,
        moul_core::api::parse_query(&format!("x={secret}")).pop()?.1,
    ))
}

/// `POST /api/oauth2/token`
async fn token(State(state): State<AppState>, req: Request) -> Response {
    let (headers, b) = body_json(req).await;
    let (mut client_id, mut secret) = (text(&b, "client_id"), text(&b, "client_secret"));
    if let Some((id, s)) = basic_auth(&headers) {
        if client_id.is_empty() {
            client_id = id;
        }
        if secret.is_empty() {
            secret = s;
        }
    }
    let Some(c) = client(&state, &client_id).await else {
        return oauth_error(401, "invalid_client", "unknown client");
    };
    if let Some(hash) = &c.secret_hash
        && !crypto::constant_time_eq(
            hash.as_bytes(),
            crypto::sha256_hex(secret.as_bytes()).as_bytes(),
        )
    {
        return oauth_error(
            401,
            "invalid_client",
            "the client secret is wrong or missing",
        );
    }
    let ttl = state.config.token_ttl_ms / 1000;
    match text(&b, "grant_type").as_str() {
        "authorization_code" => {
            let code = text(&b, "code");
            let taken = state
                .auth
                .codes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&code);
            let Some(k) = taken.filter(|k| k.expires > now()) else {
                return oauth_error(400, "invalid_grant", "the code is unknown, used or expired");
            };
            if k.client != client_id {
                return oauth_error(
                    400,
                    "invalid_grant",
                    "the code was issued to another client",
                );
            }
            let redirect = text(&b, "redirect_uri");
            if !redirect.is_empty() && !same_uri(&redirect, &k.redirect) {
                return oauth_error(400, "invalid_grant", "redirect_uri does not match");
            }
            if !k.challenge.is_empty() {
                let verifier = text(&b, "code_verifier");
                if verifier.is_empty() || moul_core::oauth::challenge(&verifier) != k.challenge {
                    return oauth_error(400, "invalid_grant", "PKCE verification failed");
                }
            }
            let Some(refresh) = issue_refresh(&state, &client_id, &k.root, &k.scope).await else {
                return oauth_error(500, "server_error", "could not issue a refresh token");
            };
            no_store(
                axum::Json(json!({
                    "access_token": k.token, "token_type": "Bearer", "expires_in": ttl,
                    "scope": k.scope, "refresh_token": refresh,
                }))
                .into_response(),
            )
        }
        "refresh_token" => {
            let hash = crypto::sha256_hex(text(&b, "refresh_token").as_bytes());
            let id = client_id.clone();
            let found = state
                .db
                .run(move |db| {
                    let conn = db.engine.store().conn();
                    let row: Option<(String, String, String)> = conn
                        .query_row(
                            "DELETE FROM writer._oauth_refresh WHERE tokenHash = ?1 AND client = ?2 AND expiresAt > ?3
                             RETURNING rootUser, scope, client",
                            params![hash, id, moul_core::time::rfc3339(now())],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()
                        .ok()
                        .flatten();
                    row.and_then(|(root, scope, _)| db.engine.issue_root_token(&root).map(|t| (t, root, scope)))
                })
                .await
                .ok()
                .flatten();
            let Some((access, root, scope)) = found else {
                return oauth_error(
                    400,
                    "invalid_grant",
                    "the refresh token is unknown, used or expired",
                );
            };
            let Some(refresh) = issue_refresh(&state, &client_id, &root, &scope).await else {
                return oauth_error(500, "server_error", "could not issue a refresh token");
            };
            no_store(
                axum::Json(json!({
                    "access_token": access, "token_type": "Bearer", "expires_in": ttl,
                    "scope": scope, "refresh_token": refresh,
                }))
                .into_response(),
            )
        }
        "" => oauth_error(400, "invalid_request", "grant_type is required"),
        other => oauth_error(
            400,
            "unsupported_grant_type",
            &format!("{other} is not supported"),
        ),
    }
}

/// `POST /api/oauth2/device/authorize`
async fn device_authorize(State(state): State<AppState>, req: Request) -> Response {
    let b0 = base(&state, &req);
    let (_, b) = body_json(req).await;
    if text(&b, "client_id").is_empty() {
        return oauth_error(400, "invalid_request", "client_id is required");
    }
    const ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ23456789";
    let mut devices = state.auth.devices.lock().unwrap_or_else(|e| e.into_inner());
    let t = now();
    devices.retain(|_, d| d.expires > t);
    let user_code = loop {
        let mut raw = [0u8; 8];
        getrandom::fill(&mut raw).expect("the operating system supplies randomness");
        let s: String = raw
            .iter()
            .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
            .collect();
        let code = format!("{}-{}", &s[..4], &s[4..]);
        if !devices.values().any(|d| d.user_code == code) {
            break code;
        }
    };
    let device_code = random(32);
    devices.insert(
        device_code.clone(),
        Device {
            user_code: user_code.clone(),
            expires: t + DEVICE_TTL_MS,
            last_poll: 0,
            token: None,
        },
    );
    axum::Json(json!({
        "device_code": device_code,
        "user_code": user_code,
        "verification_uri": format!("{b0}/device"),
        "verification_uri_complete": format!("{b0}/device?user_code={user_code}"),
        "expires_in": DEVICE_TTL_MS / 1000,
        "interval": DEVICE_INTERVAL,
    }))
    .into_response()
}

/// `POST /api/oauth2/device/token`
async fn device_token(State(state): State<AppState>, req: Request) -> Response {
    let (_, b) = body_json(req).await;
    if text(&b, "grant_type") != "urn:ietf:params:oauth:grant-type:device_code" {
        return oauth_error(
            400,
            "unsupported_grant_type",
            "use urn:ietf:params:oauth:grant-type:device_code",
        );
    }
    let code = text(&b, "device_code");
    if code.is_empty() {
        return oauth_error(400, "invalid_request", "device_code is required");
    }
    let mut devices = state.auth.devices.lock().unwrap_or_else(|e| e.into_inner());
    let t = now();
    let Some(d) = devices.get_mut(&code).filter(|d| d.expires > t) else {
        return oauth_error(
            400,
            "expired_token",
            "the device code is unknown or expired",
        );
    };
    if let Some(token) = d.token.take() {
        devices.remove(&code);
        return no_store(
            axum::Json(json!({
                "access_token": token, "token_type": "Bearer",
                "expires_in": state.config.token_ttl_ms / 1000,
            }))
            .into_response(),
        );
    }
    let too_soon = t - d.last_poll < DEVICE_INTERVAL * 1000;
    d.last_poll = t;
    if too_soon {
        return oauth_error(400, "slow_down", "poll no more often than the interval");
    }
    oauth_error(
        400,
        "authorization_pending",
        "the code has not been approved yet",
    )
}

fn device_form(user_code: &str, err: Option<&str>, status: u16) -> Response {
    let body = format!(
        r#"<h1>Authorize a device</h1>
<p>Enter the code the device shows, and sign in as a root user to let it act as you.</p>
{err}<form method="post" action="/device/verify">
<label for="user_code">Code</label><input id="user_code" name="user_code" value="{code}" autocomplete="one-time-code" required>
<label for="identity">Root user name or email</label><input id="identity" name="identity" autocomplete="username" required>
<label for="password">Password</label><input id="password" name="password" type="password" autocomplete="current-password" required>
<button type="submit">Authorize</button></form>"#,
        code = esc(user_code),
        err = err
            .map(|e| format!(r#"<p class="err">{}</p>"#, esc(e)))
            .unwrap_or_default(),
    );
    page(status, "Authorize Device - Moul", &body)
}

/// `GET /device`
async fn device_page(req: Request) -> Response {
    let q = moul_core::api::parse_query(req.uri().query().unwrap_or(""));
    let code = q
        .iter()
        .find(|(k, _)| k == "user_code")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let collection = q
        .iter()
        .find(|(k, _)| k == "auth_moul")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    if !collection.is_empty() && collection != "_rootUsers" {
        return device_form(
            &code,
            Some("Device authorization is only supported for the root account"),
            400,
        );
    }
    device_form(&code, None, 200)
}

fn normalize_code(c: &str) -> String {
    c.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// `POST /device/verify`
async fn device_verify(State(state): State<AppState>, req: Request) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0.to_string());
    let wants_json = ["accept", "content-type"].iter().any(|h| {
        req.headers()
            .get(*h)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("application/json"))
    });
    let (_, b) = body_json(req).await;
    let fail = |m: &str| {
        if wants_json {
            error(400, m)
        } else {
            device_form(&text(&b, "user_code"), Some(m), 400)
        }
    };
    let collection = text(&b, "auth_moul");
    if !collection.is_empty() && collection != "_rootUsers" {
        return fail("Device authorization is only supported for the root account");
    }
    let user_code = normalize_code(&text(&b, "user_code"));
    if user_code.is_empty() {
        return fail("user_code is required");
    }
    let (identity, password) = (text(&b, "identity"), text(&b, "password"));
    if identity.is_empty() || password.is_empty() {
        return fail("identity and password are required");
    }
    let known = {
        let devices = state.auth.devices.lock().unwrap_or_else(|e| e.into_inner());
        devices
            .iter()
            .find(|(_, d)| normalize_code(&d.user_code) == user_code && d.expires > now())
            .map(|(k, d)| (k.clone(), d.token.is_some()))
    };
    let Some((device_code, approved)) = known else {
        return fail("Invalid or expired user code");
    };
    if approved {
        return fail("This code was already approved");
    }
    let (token, _) = match root_sign_in(&state, &identity, &password, ip).await {
        Ok(t) => t,
        Err(_) => return fail("Invalid credentials"),
    };
    if let Some(d) = state
        .auth
        .devices
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&device_code)
    {
        d.token = Some(token.clone());
    }
    if wants_json {
        return axum::Json(json!({"success": true, "token": token})).into_response();
    }
    page(
        200,
        "Device Authorized - Moul",
        "<h1>Device authorized</h1><p>You can close this page and go back to the device.</p>",
    )
}

/// The headers the engine reads credentials from (for MCP).
pub fn credential_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    engine_headers(headers)
}
