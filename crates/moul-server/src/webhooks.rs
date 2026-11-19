//! Calling webhooks: delivery for the dispatcher, and the test endpoints.
//!
//! A call is a `POST` of the JSON payload the write recorded, with the Go
//! engine's headers: `X-Moul-Event`, `X-Moul-Webhook-ID`, `X-Moul-Timestamp`
//! and, when the webhook has a secret, `X-Moul-Signature`, the lowercase hex
//! HMAC-SHA256 of the body under the secret. `X-Moul-Delivery` carries the
//! call's own id, the same on every retry, so a receiver can tell a retry
//! from a new event. Any 2xx answer delivers it.

use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::http::{AppState, ClientIp, error, require_admin};

/// The signature of `body` under `secret`.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    m.update(body);
    moul_core::ids::hex(&m.finalize().into_bytes())
}

/// What one call did.
#[derive(Debug)]
pub struct Called {
    /// The status, if the server answered.
    pub status: Option<u16>,
    /// The start of the answer's body.
    pub body: String,
    /// What went wrong, if anything.
    pub error: Option<String>,
    /// How long it took.
    pub millis: u128,
}

/// One call to make.
#[derive(Clone, Copy, Debug)]
pub struct Call<'a> {
    /// Where to.
    pub url: &'a str,
    /// `X-Moul-Event`.
    pub event: &'a str,
    /// `X-Moul-Webhook-ID`.
    pub webhook: &'a str,
    /// `X-Moul-Delivery`: the same on every retry.
    pub delivery: &'a str,
    /// `X-Moul-Timestamp`: when the change happened.
    pub timestamp: &'a str,
    /// Signs the body when set.
    pub secret: Option<&'a str>,
    /// The JSON body.
    pub payload: &'a str,
}

/// Makes a call.
pub async fn call(state: &AppState, c: Call<'_>) -> Called {
    let Call {
        url,
        event,
        webhook,
        delivery,
        timestamp,
        secret,
        payload,
    } = c;
    let started = Instant::now();
    let allow = state.config.allow_private_calls;
    let url = match crate::net::check_url(url, allow) {
        Ok(u) => u,
        Err(e) => {
            return Called {
                status: None,
                body: String::new(),
                error: Some(e),
                millis: 0,
            };
        }
    };
    let mut req = state
        .outbound
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "moul-webhook/1.0")
        .header("X-Moul-Event", event)
        .header("X-Moul-Webhook-ID", webhook)
        .header("X-Moul-Timestamp", timestamp)
        .header("X-Moul-Delivery", delivery)
        .timeout(std::time::Duration::from_secs(10))
        .body(payload.to_owned());
    if let Some(s) = secret.filter(|s| !s.is_empty()) {
        req = req.header("X-Moul-Signature", signature(s, payload.as_bytes()));
    }
    match req.send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let body: String = text
                .chars()
                .take(1024)
                .collect::<String>()
                .trim()
                .to_owned();
            Called {
                status: Some(status),
                error: (!(200..300).contains(&status)).then(|| format!("HTTP {status}")),
                body,
                millis: started.elapsed().as_millis(),
            }
        }
        Err(e) => Called {
            status: None,
            body: String::new(),
            error: Some(e.to_string()),
            millis: started.elapsed().as_millis(),
        },
    }
}

fn ping(name: &str) -> String {
    json!({
        "event": "ping",
        "moul": name,
        "record": {"test": true, "message": "Moul Webhook Ping Test"},
        "timestamp": moul_core::time::rfc3339(moul_vfs::system_clock()),
    })
    .to_string()
}

fn result(c: Called) -> Response {
    Json(json!({
        "status_code": c.status.unwrap_or(0),
        "duration_ms": c.millis as u64,
        "response": c.body,
        "error": c.error.clone().unwrap_or_default(),
        "success": c.error.is_none(),
    }))
    .into_response()
}

/// `POST /api/moul/:name/webhooks/test` with a webhook in the body.
pub(crate) async fn test(
    State(state): State<AppState>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0);
    if let Err(r) = require_admin(&state, req.headers(), ip).await {
        return r;
    }
    let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let hook: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let Some(url) = hook["url"]
        .as_str()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    else {
        return error(400, "Invalid webhook body or missing URL");
    };
    let n = name.clone();
    let exists = state
        .db
        .run(move |db| {
            db.engine
                .handle(
                    moul_core::Request::get(&format!("/api/moul/{}", moul_core::oauth::encode(&n)))
                        .owner(),
                )
                .status
                == 200
        })
        .await
        .unwrap_or(false);
    if !exists {
        return error(404, "Not found");
    }
    let payload = ping(&name);
    let ts = moul_core::time::rfc3339(moul_vfs::system_clock());
    let id = hook["id"].as_str().unwrap_or("test").to_owned();
    result(
        call(
            &state,
            Call {
                url,
                event: "ping",
                webhook: &id,
                delivery: "test",
                timestamp: &ts,
                secret: hook["secret"].as_str(),
                payload: &payload,
            },
        )
        .await,
    )
}

/// `POST /api/moul/:name/webhooks/:id/test`
pub(crate) async fn test_one(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    req: Request,
) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0);
    if let Err(r) = require_admin(&state, req.headers(), ip).await {
        return r;
    }
    let (n, i) = (name.clone(), id.clone());
    let target = state
        .db
        .run(move |db| db.engine.webhook_target(&n, &i))
        .await
        .ok()
        .flatten();
    let Some((url, secret)) = target else {
        return error(404, "Webhook not found");
    };
    let payload = ping(&name);
    let ts = moul_core::time::rfc3339(moul_vfs::system_clock());
    result(
        call(
            &state,
            Call {
                url: &url,
                event: "ping",
                webhook: &id,
                delivery: "test",
                timestamp: &ts,
                secret: secret.as_deref(),
                payload: &payload,
            },
        )
        .await,
    )
}
