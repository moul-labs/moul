//! The HTTP side: the router, the middleware every request passes, and the
//! fallback that hands a request to the engine.
//!
//! Every request first has its client's address worked out (believing
//! `X-Forwarded-For` only from trusted proxies), is refused if it carries
//! credentials in the query string, gets its CORS answer, and is counted
//! against the rate limits. Then the server's own routes (live streams,
//! MCP, OAuth, logs, docs) answer it, or the engine does. Afterwards it is
//! written to the request log.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use moul_core::{ApiError, ResponseBody};
use serde_json::{Value, json};

use crate::config::{ServerConfig, canonical};
use crate::db::DbHandle;
use crate::{auth_server, docs, limits, logs, mcp, oauth, realtime, webhooks};

/// What every handler shares.
#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// The parts of [`AppState`].
pub struct Inner {
    /// The engine's thread.
    pub db: DbHandle,
    /// How the server was set up.
    pub config: ServerConfig,
    /// The admin key in force.
    pub admin_key: String,
    /// The settings, as last read from the engine.
    pub settings: RwLock<Arc<BTreeMap<String, String>>>,
    /// Single-use tickets for live streams.
    pub tickets: realtime::Tickets,
    /// Rate limits and connection counts.
    pub limiter: limits::Limiter,
    /// The request log's queue.
    pub log: logs::RequestLog,
    /// System metrics.
    pub metrics: logs::Metrics,
    /// HTTPS calls out (webhooks, OAuth providers, mail), refusing private
    /// addresses.
    pub outbound: reqwest::Client,
    /// The OAuth authorization server's clients, codes and device codes.
    pub auth: auth_server::Store,
    /// When the server started.
    pub started: Instant,
}

impl AppState {
    /// One setting, from the cache.
    pub fn setting(&self, key: &str) -> String {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Reads the settings again from the engine, and applies what the
    /// server caches (the rate limits).
    pub async fn reload_settings(&self) {
        if let Ok(s) = self.db.run(|db| db.engine.settings()).await {
            self.limiter.configure(&s);
            *self.settings.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(s);
        }
    }

    /// The name MCP clients and the consent page show.
    pub fn app_name(&self) -> String {
        self.config
            .app_name
            .clone()
            .unwrap_or_else(|| self.setting("app_name"))
    }
}

/// The client's address, as the middleware worked it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

/// The router of a server.
pub fn router(state: AppState) -> Router {
    router_with(state, Router::new(), true)
}

/// A router with the middleware every request passes, around `extra`
/// routes and, with `engine_api`, the server's own routes and the engine's
/// API. Moul Hub serves its routes this way, without the engine's API.
pub fn router_with(state: AppState, extra: Router<AppState>, engine_api: bool) -> Router {
    let limit = state.config.max_body_bytes;
    if !engine_api {
        return extra
            .merge(docs::health_routes())
            .fallback(|| async { error(404, "Not Found") })
            .layer(middleware::from_fn_with_state(state.clone(), around))
            .layer(DefaultBodyLimit::max(limit))
            .with_state(state);
    }
    Router::new()
        .merge(extra)
        .route("/api/realtime/ticket", post(realtime::ticket))
        .route("/api/moul/subscribe", get(realtime::subscribe_all))
        .route("/api/moul/{name}/subscribe", get(realtime::subscribe))
        .route("/api/mcp", any(mcp::http))
        .route("/api/mcp/{*rest}", any(mcp::http))
        .route("/mcp", any(mcp::http))
        .route("/mcp/{*rest}", any(mcp::http))
        .route(
            "/api/moul/{name}/oauth2/{provider}/callback",
            get(oauth::callback).post(oauth::callback),
        )
        .route(
            "/api/moul/{name}/auth-with-oauth2",
            post(oauth::with_oauth2),
        )
        .route("/api/moul/{name}/webhooks/test", post(webhooks::test))
        .route(
            "/api/moul/{name}/webhooks/{id}/test",
            post(webhooks::test_one),
        )
        .route("/api/requests", get(logs::requests))
        .route("/api/requests/{id}", get(logs::request))
        .route("/api/visits", get(logs::visits))
        .route("/api/visits/{id}", get(logs::visit))
        .route(
            "/api/system/metrics",
            get(logs::metrics_get).post(logs::metrics_post),
        )
        .merge(auth_server::routes())
        .merge(docs::routes())
        .fallback(engine)
        .layer(middleware::from_fn_with_state(state.clone(), around))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

/// Works out the client's address: the peer, unless the peer is a trusted
/// proxy, in which case the configured header or the right-most untrusted
/// hop of `X-Forwarded-For`.
pub fn client_ip(config: &ServerConfig, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    let peer = canonical(peer);
    let trusted = |ip: IpAddr| config.trusted_proxies.iter().any(|t| t.contains(ip));
    if !trusted(peer) {
        return peer;
    }
    let parse = |s: &str| -> Option<IpAddr> {
        let s = s.trim();
        let s = s
            .strip_prefix('[')
            .map_or(s, |r| r.split(']').next().unwrap_or(r));
        s.parse::<IpAddr>()
            .ok()
            .or_else(|| s.parse::<SocketAddr>().ok().map(|a| a.ip()))
            .map(canonical)
    };
    if let Some(h) = &config.client_ip_header {
        return headers
            .get(h.as_str())
            .and_then(|v| v.to_str().ok())
            .and_then(parse)
            .unwrap_or(peer);
    }
    let hops: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .collect();
    let mut last = peer;
    for hop in hops.iter().rev() {
        let Some(ip) = parse(hop) else {
            return peer;
        };
        if !trusted(ip) {
            return ip;
        }
        last = ip;
    }
    last
}

/// The scheme and host a request was made to, for links back to it.
pub fn base_url(state: &AppState, headers: &HeaderMap, peer_trusted: bool) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty());
    let forwarded = |name: &str| {
        peer_trusted
            .then(|| headers.get(name).and_then(|v| v.to_str().ok()))
            .flatten()
            .map(|v| v.split(',').next().unwrap_or("").trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    match host {
        Some(h) => {
            let proto = forwarded("x-forwarded-proto").unwrap_or_else(|| "http".into());
            let host = forwarded("x-forwarded-host").unwrap_or_else(|| h.to_owned());
            format!("{proto}://{host}")
        }
        None => state.config.public_url(),
    }
}

fn json_response(status: u16, body: Value) -> Response {
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

/// An error answer: `{"message": …}`.
pub fn error(status: u16, message: &str) -> Response {
    json_response(status, json!({"message": message}))
}

/// The messages of a refusal for want of admin rights, which come with a
/// pointer to the OAuth metadata so an MCP client can sign in.
const CHALLENGED: [&str; 2] = [
    "Admin authentication required (root user token or admin key)",
    "Authentication required (either valid JWT user or admin key)",
];

/// Turns the engine's answer into an HTTP response.
pub fn to_http(r: moul_core::Response, challenge: Option<&str>) -> Response {
    let status = r.status;
    let mut resp = match &r.body {
        ResponseBody::Empty => Response::new(Body::empty()),
        ResponseBody::Json(v) => {
            let mut resp = Response::new(Body::from(serde_json::to_vec(v).unwrap_or_default()));
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            resp
        }
        ResponseBody::Bytes { data, .. } => Response::new(Body::from(data.clone())),
    };
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (k, v) in &r.headers {
        if k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(k.as_str()),
            HeaderValue::try_from(v.as_str()),
        ) {
            resp.headers_mut().append(name, value);
        }
    }
    if status == 401
        && CHALLENGED.contains(&r.message())
        && let Some(c) = challenge.and_then(|c| HeaderValue::try_from(c).ok())
    {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, c);
    }
    resp
}

/// The `WWW-Authenticate` challenge for a request to `base`.
pub fn challenge_for(base: &str) -> String {
    let host = base.split_once("://").map_or(base, |(_, h)| h);
    format!(
        "Bearer realm=\"{host}\", error=\"unauthorized\", resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
    )
}

/// Headers as the engine takes them.
pub fn engine_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// Whether the peer that sent a request is a trusted proxy.
pub fn peer_trusted(state: &AppState, req: &Request) -> bool {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    state
        .config
        .trusted_proxies
        .iter()
        .any(|t| t.contains(canonical(peer)))
}

/// The fallback: the engine answers. Moul Hub routes a few of the engine's
/// owner-only paths (settings, the inbox) here too.
pub async fn engine(State(state): State<AppState>, req: Request) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0);
    let trusted = peer_trusted(&state, &req);
    let base = base_url(&state, req.headers(), trusted);
    let (parts, body) = req.into_parts();
    let bytes = match to_bytes(body, state.config.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => return error(413, "Request body too large"),
    };
    let mut r = moul_core::Request::from_http(
        parts.method.as_str(),
        parts.uri.path(),
        parts.uri.query().unwrap_or(""),
        engine_headers(&parts.headers),
        &bytes,
    );
    r.remote = ip.map(|ip| ip.to_string());
    let path = r.path.clone();
    let method = r.method.clone();
    let resp = state.db.handle(r).await;
    let settings_changed = resp.status == 200
        && ((method == "PATCH" && path.trim_end_matches('/') == "/api/settings")
            || (method == "POST" && path.trim_end_matches('/').ends_with("/reload")));
    if settings_changed {
        state.reload_settings().await;
    }
    to_http(resp, Some(&challenge_for(&base)))
}

/// Query parameters that would carry credentials into logs.
fn query_credentials(query: &str) -> Option<&'static str> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if v.is_empty() {
            continue;
        }
        if k == "token" {
            return Some(
                "Authentication via ?token= query parameter is disabled for security reasons. Provide your credentials via Authorization header (Bearer <token>) or request a single-use ticket via POST /api/realtime/ticket.",
            );
        }
        if ["admin_key", "adminKey", "api_key"].contains(&k) {
            return Some(
                "Authentication via URL query parameters is disabled for security reasons. Provide your admin key via the X-Admin-Key header or request a single-use ticket via POST /api/realtime/ticket.",
            );
        }
    }
    None
}

const CORS_METHODS: &str = "GET, POST, PATCH, PUT, DELETE, OPTIONS, HEAD";
const CORS_HEADERS: &str = "Authorization, Content-Type, X-Admin-Key, X-Visit-Token, X-Visitor-Token, Mcp-Session-Id, Mcp-Protocol-Version, Last-Event-ID, Accept, Moul-Request-Id, Moul-Budget, If-Match, If-None-Match";
const CORS_EXPOSE: &str =
    "Moul-Reads, Moul-Change, Moul-Request-Id, Moul-Replayed, Mcp-Session-Id, Retry-After, ETag";

/// The `Access-Control-Allow-Origin` value for `origin`, if it is allowed.
fn allowed_origin(config: &ServerConfig, origin: &str) -> Option<String> {
    if config.cors_origins.is_empty() || config.cors_origins.iter().any(|o| o == "*") {
        return Some("*".into());
    }
    config
        .cors_origins
        .iter()
        .find(|o| o.eq_ignore_ascii_case(origin))
        .cloned()
}

/// The middleware every request passes.
async fn around(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let started = Instant::now();
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let ip = client_ip(&state.config, peer, req.headers());
    req.extensions_mut().insert(ClientIp(ip));
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let allow = origin
        .as_deref()
        .and_then(|o| allowed_origin(&state.config, o));
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    let mut resp = if method == Method::OPTIONS
        && origin.is_some()
        && req
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NO_CONTENT;
        if allow.is_some() {
            let h = r.headers_mut();
            h.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static(CORS_METHODS),
            );
            h.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static(CORS_HEADERS),
            );
            h.insert(
                header::ACCESS_CONTROL_MAX_AGE,
                HeaderValue::from_static("600"),
            );
        }
        r
    } else if let Some(m) = query_credentials(req.uri().query().unwrap_or("")) {
        error(400, m)
    } else if let Some(refusal) = {
        let headers = req.headers().clone();
        state
            .limiter
            .check(&state, method.as_str(), &path, &headers, ip)
            .await
    } {
        refusal
    } else {
        next.run(req).await
    };

    let h = resp.headers_mut();
    h.append(header::VARY, HeaderValue::from_static("Origin"));
    if let Some(a) = allow.and_then(|a| HeaderValue::try_from(a).ok()) {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, a);
        h.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(CORS_EXPOSE),
        );
    }
    state.log.record(
        &state,
        &method,
        &path,
        resp.status().as_u16(),
        started.elapsed(),
    );
    resp
}

/// Refuses a request that is not from the owner: the admin key or a root
/// user. Returns the engine's own refusal, with the challenge.
pub async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    ip: Option<IpAddr>,
) -> Result<(), Response> {
    let creds = engine_headers(headers);
    let remote = ip.map(|i| i.to_string());
    let who = state
        .db
        .run(move |db| db.engine.identity(&creds, remote.as_deref()))
        .await
        .map_err(|_| error(503, "The database is closed"))?;
    match who {
        Ok(id) if id.admin => Ok(()),
        Ok(id) if id.user.is_some() => Err(error(403, "Root or admin access required")),
        Ok(_) => {
            let base = base_url(state, headers, false);
            Err(to_http(
                ApiError::unauthorized(CHALLENGED[0]).into_response(),
                Some(&challenge_for(&base)),
            ))
        }
        Err(r) => Err(to_http(r, None)),
    }
}

impl IntoResponse for crate::db::Stopped {
    fn into_response(self) -> Response {
        error(503, "The database is closed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_trusted;

    #[test]
    fn client_addresses_come_from_trusted_proxies_only() {
        let mut c = ServerConfig::default();
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 10.0.0.2"),
        );
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        // 10.0.0.2 is not trusted by default: it is the client.
        assert_eq!(
            client_ip(&c, local, &h),
            "10.0.0.2".parse::<IpAddr>().unwrap()
        );
        c.trusted_proxies = parse_trusted("loopback,private").unwrap();
        assert_eq!(
            client_ip(&c, local, &h),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        // A peer that is not a proxy is the client, whatever it says.
        let far: IpAddr = "198.51.100.1".parse().unwrap();
        assert_eq!(client_ip(&c, far, &h), far);
        assert!(query_credentials("token=abc").is_some());
        assert!(query_credentials("token=").is_none());
        assert!(query_credentials("filter=x&adminKey=k").is_some());
    }
}
