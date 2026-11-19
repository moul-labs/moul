//! Live record events as server-sent events, in the Go engine's format.
//!
//! | Path | |
//! | --- | --- |
//! | `POST /api/realtime/ticket` | a single-use ticket, valid 30 seconds, for a client that cannot send headers (`EventSource`) |
//! | `GET /api/moul/:name/subscribe` | one collection's events: `?event=create,update`, `?id=<record>`, `?ticket=` |
//! | `GET /api/moul/subscribe` | every collection's (or `?moul=<name>`'s), for the owner |
//!
//! A stream starts with `event: connected`, carries one `event: <action>`
//! per record change the subscriber may see, sends `: ping` every 15
//! seconds, and ends with `event: auth_expired` when the subscriber's token
//! expires or is revoked, or `event: slow_consumer` when it falls 256
//! events behind.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use futures_util::stream;
use moul_core::Subscription;
use serde::Deserialize;
use serde_json::json;

use crate::db::{StreamMsg, StreamRx};
use crate::http::{AppState, ClientIp, engine_headers, error, to_http};

/// How long a ticket lasts.
pub const TICKET_TTL_MS: i64 = 30_000;

/// How often an idle stream sends a comment, so proxies keep it open.
pub const PING_EVERY: Duration = Duration::from_secs(15);

struct Ticket {
    credentials: Vec<(String, String)>,
    reveal: String,
    expires: i64,
}

/// Tickets not yet used.
#[derive(Default)]
pub struct Tickets(Mutex<HashMap<String, Ticket>>);

impl Tickets {
    fn issue(&self, credentials: Vec<(String, String)>, reveal: String, now: i64) -> String {
        let mut b = [0u8; 24];
        getrandom::fill(&mut b).expect("the operating system supplies randomness");
        let id = format!("tkt_{}", moul_core::ids::hex(&b));
        let mut t = self.0.lock().unwrap_or_else(|e| e.into_inner());
        t.retain(|_, v| v.expires > now);
        t.insert(
            id.clone(),
            Ticket {
                credentials,
                reveal,
                expires: now + TICKET_TTL_MS,
            },
        );
        id
    }

    fn take(&self, id: &str, now: i64) -> Option<Ticket> {
        let mut t = self.0.lock().unwrap_or_else(|e| e.into_inner());
        t.remove(id).filter(|v| v.expires > now)
    }
}

/// The headers that carry credentials.
fn credentials(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    engine_headers(headers)
        .into_iter()
        .filter(|(k, _)| {
            k.eq_ignore_ascii_case("authorization") || k.eq_ignore_ascii_case("x-admin-key")
        })
        .collect()
}

const REVEAL_REFUSED: &str = "The reveal parameter is not permitted on realtime broadcast streams";

/// `POST /api/realtime/ticket`
pub(crate) async fn ticket(State(state): State<AppState>, req: Request) -> Response {
    let creds = credentials(req.headers());
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0.to_string());
    let reveal = query_value(req.uri().query(), "reveal");
    let probe = creds.clone();
    let who = match state
        .db
        .run(move |db| db.engine.identity(&probe, ip.as_deref()))
        .await
    {
        Ok(Ok(who)) => who,
        Ok(Err(r)) => return to_http(r, None),
        Err(e) => return axum::response::IntoResponse::into_response(e),
    };
    if !who.admin && who.user.is_none() {
        return error(401, "Authentication required to request a realtime ticket");
    }
    if !reveal.is_empty() && !who.admin {
        return error(400, REVEAL_REFUSED);
    }
    let now = moul_vfs::system_clock();
    let id = state.tickets.issue(creds, reveal, now);
    axum::Json(json!({
        "ticket": id,
        "expiresAt": moul_core::time::rfc3339((now + TICKET_TTL_MS) / 1000 * 1000),
        "expiresIn": TICKET_TTL_MS / 1000,
    }))
    .into_response_ok()
}

trait IntoResponseOk {
    fn into_response_ok(self) -> Response;
}

impl<T: axum::response::IntoResponse> IntoResponseOk for T {
    fn into_response_ok(self) -> Response {
        self.into_response()
    }
}

fn query_value(query: Option<&str>, key: &str) -> String {
    moul_core::api::parse_query(query.unwrap_or(""))
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_default()
}

/// The query of a subscribe request.
#[derive(Deserialize, Default)]
pub(crate) struct SubQuery {
    ticket: Option<String>,
    event: Option<String>,
    events: Option<String>,
    id: Option<String>,
    record_id: Option<String>,
    reveal: Option<String>,
    moul: Option<String>,
    mouls: Option<String>,
}

/// `GET /api/moul/:name/subscribe`
pub(crate) async fn subscribe(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<SubQuery>,
    req: Request,
) -> Response {
    let record =
        q.id.clone()
            .or(q.record_id.clone())
            .filter(|s| !s.is_empty());
    open(state, name, record, q, req).await
}

/// `GET /api/moul/subscribe`
pub(crate) async fn subscribe_all(
    State(state): State<AppState>,
    Query(q): Query<SubQuery>,
    req: Request,
) -> Response {
    let name = q
        .mouls
        .clone()
        .or(q.moul.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "*".into());
    let record = q.id.clone().filter(|s| !s.is_empty());
    open(state, name, record, q, req).await
}

async fn open(
    state: AppState,
    name: String,
    record: Option<String>,
    q: SubQuery,
    req: Request,
) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0);
    let now = moul_vfs::system_clock();
    // A collection named in the path must exist, whoever asks.
    if name != "*" && !name.contains(',') {
        let n = name.clone();
        let exists = state
            .db
            .run(move |db| {
                db.engine
                    .handle(
                        moul_core::Request::get(&format!(
                            "/api/moul/{}",
                            moul_core::oauth::encode(&n)
                        ))
                        .owner(),
                    )
                    .status
                    == 200
            })
            .await
            .unwrap_or(false);
        if !exists {
            return error(404, &format!("Moul collection \"{name}\" not found"));
        }
    }
    let mut creds = credentials(req.headers());
    let mut reveal = q.reveal.clone().unwrap_or_default();
    if let Some(t) = q.ticket.as_deref().filter(|t| !t.is_empty()) {
        let Some(ticket) = state.tickets.take(t, now) else {
            return error(401, "Invalid or expired realtime ticket");
        };
        if !ticket.credentials.is_empty() {
            creds = ticket.credentials;
        }
        if reveal.is_empty() {
            reveal = ticket.reveal;
        }
    }
    let probe = creds.clone();
    let remote = ip.map(|i| i.to_string());
    let who = match state
        .db
        .run(move |db| db.engine.identity(&probe, remote.as_deref()))
        .await
    {
        Ok(Ok(who)) => who,
        Ok(Err(r)) => return to_http(r, None),
        Err(e) => return axum::response::IntoResponse::into_response(e),
    };
    if !reveal.is_empty() && !who.admin {
        return error(400, REVEAL_REFUSED);
    }
    let events: Vec<String> = q
        .event
        .or(q.events)
        .unwrap_or_default()
        .split(',')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    // Several collections at once: the owner's stream of all, filtered here.
    let only: Option<Vec<String>> = name.contains(',').then(|| {
        name.split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let user_key = who
        .user
        .as_ref()
        .map(|u| format!("{}:{}", u.collection, u.id));
    let slot = match state.limiter.open_stream(
        ip.unwrap_or(std::net::IpAddr::from([127, 0, 0, 1])),
        user_key.as_deref(),
        who.admin,
        &name,
        req.uri().path(),
    ) {
        Ok(slot) => Slot {
            state: state.clone(),
            keys: slot.into_keys(),
        },
        Err(why) => return error(429, why),
    };
    let sub = Subscription {
        collection: if only.is_some() {
            "*".into()
        } else {
            name.clone()
        },
        record,
        events,
        credentials: creds,
        owner: false,
    };
    let opened = state.db.run(move |db| db.subscribe(sub)).await;
    let rx = match opened {
        Ok(Ok(rx)) => rx,
        Ok(Err(r)) => return to_http(r, None),
        Err(e) => return axum::response::IntoResponse::into_response(e),
    };
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).expect("the operating system supplies randomness");
    let hello = format!(
        "event: connected\ndata: {}\n\n",
        json!({"client_id": moul_core::ids::hex(&id), "moul": name, "status": "connected"})
    );
    let body = Body::from_stream(events_stream(state, rx, slot, hello, only));
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

/// A live stream's place in the connection counts, given back when dropped:
/// when the stream ends, or when the client leaves before it starts.
struct Slot {
    state: AppState,
    keys: Vec<String>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.state.limiter.release(&self.keys);
    }
}

/// What the stream holds while it is open; ends the subscription when the
/// client goes away.
struct Open {
    state: AppState,
    rx: StreamRx,
    _slot: Slot,
    hello: Option<String>,
    only: Option<Vec<String>>,
    ping: tokio::time::Interval,
    done: bool,
}

impl Drop for Open {
    fn drop(&mut self) {
        let id = self.rx.id;
        let _ = self.state.db.send(Box::new(move |db| db.unsubscribe(id)));
    }
}

fn events_stream(
    state: AppState,
    rx: StreamRx,
    slot: Slot,
    hello: String,
    only: Option<Vec<String>>,
) -> impl futures_util::Stream<Item = Result<Bytes, Infallible>> {
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let open = Open {
        state,
        rx,
        _slot: slot,
        hello: Some(hello),
        only,
        ping,
        done: false,
    };
    stream::unfold(open, |mut o| async move {
        if o.done {
            return None;
        }
        if let Some(h) = o.hello.take() {
            return Some((Ok(Bytes::from(h)), o));
        }
        loop {
            tokio::select! {
                msg = o.rx.rx.recv() => {
                    let frame = match msg {
                        Some(StreamMsg::Event(e)) => {
                            if o.only.as_ref().is_some_and(|names| !names.contains(&e.moul)) {
                                continue;
                            }
                            let action = serde_json::to_value(e.action).ok()
                                .and_then(|v| v.as_str().map(str::to_owned))
                                .unwrap_or_default();
                            let data = serde_json::to_string(&e).unwrap_or_default();
                            format!("event: {action}\ndata: {data}\n\n")
                        }
                        Some(StreamMsg::Closed(reason)) => {
                            o.done = true;
                            if reason.contains("shutting down") {
                                return None;
                            }
                            format!(
                                "event: auth_expired\ndata: {}\n\n",
                                json!({"message": "Token has expired or was revoked", "reason": "auth_expired"})
                            )
                        }
                        None => {
                            o.done = true;
                            if o.rx.slow.load(Ordering::SeqCst) {
                                format!(
                                    "event: slow_consumer\ndata: {}\n\n",
                                    json!({"message": "Connection terminated due to slow consumer backpressure", "reason": "slow_consumer"})
                                )
                            } else {
                                return None;
                            }
                        }
                    };
                    return Some((Ok(Bytes::from(frame)), o));
                }
                _ = o.ping.tick() => {
                    return Some((Ok(Bytes::from_static(b": ping\n\n")), o));
                }
            }
        }
    })
}
