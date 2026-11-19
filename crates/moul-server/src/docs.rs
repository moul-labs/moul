//! The server's own pages: a status at `/`, `/api/health`, the OpenAPI
//! document, a page that renders it, and `llms.txt` for agents.

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use crate::http::AppState;

const OPENAPI: &str = include_str!("../openapi.json");
const LLMS: &str = include_str!("../llms.txt");

/// The health check alone, for a host that serves no API docs.
pub fn health_routes() -> Router<AppState> {
    Router::new().route("/api/health", get(health))
}

/// The routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(root))
        .route("/api/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/docs/openapi.json", get(openapi))
        .route("/openapi.yml", get(openapi_yaml))
        .route("/docs/openapi.yml", get(openapi_yaml))
        .route("/docs", get(page))
        .route("/docs/", get(page))
        .route("/llms.txt", get(llms))
        .route("/AGENTS.md", get(llms))
}

fn typed(content_type: &'static str, body: String) -> Response {
    let mut r = Response::new(Body::from(body));
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r
}

fn spec() -> String {
    OPENAPI.replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
}

async fn root(State(state): State<AppState>) -> Response {
    axum::Json(json!({
        "name": state.app_name(),
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "docs": "/docs",
        "mcp": "/api/mcp",
    }))
    .into_response()
}

async fn health(State(state): State<AppState>) -> Response {
    let up = state.db.is_open();
    let mut r = axum::Json(json!({"status": if up { "ok" } else { "closing" }})).into_response();
    if !up {
        *r.status_mut() = axum::http::StatusCode::SERVICE_UNAVAILABLE;
    }
    r
}

async fn openapi() -> Response {
    typed("application/json; charset=utf-8", spec())
}

/// JSON is YAML too: the same document, under the name tools look for.
async fn openapi_yaml() -> Response {
    typed("text/yaml; charset=utf-8", spec())
}

async fn llms() -> Response {
    typed("text/plain; charset=utf-8", LLMS.to_owned())
}

async fn page() -> Response {
    let html = r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Moul API</title><link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.17.14/swagger-ui.css">
<style>body{margin:0;background:#fff}</style></head><body><div id="ui"></div>
<script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.17.14/swagger-ui-bundle.js" crossorigin></script>
<script>window.ui = SwaggerUIBundle({ url: "/openapi.json", dom_id: "#ui" });</script></body></html>"##;
    typed("text/html; charset=utf-8", html.to_owned())
}
