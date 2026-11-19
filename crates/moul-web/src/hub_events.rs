//! Holding a Moul Hub's event stream open while the device syncs through
//! the hub, so that it pulls as soon as another device pushes.
//!
//! The stream is `GET /api/hub/db/<name>/events`, read with `fetch` so the
//! token goes in a header. It starts with `event: connected`, sends
//! `event: head` each time a device writes `moul/head.json`, and `: ping`
//! every 15 seconds. A stream silent for 45 seconds counts as dropped. A
//! stream that held for a while and dropped is opened again after 1
//! second; one that fails or drops at once, after a wait that doubles up to
//! 30 seconds; one the hub refuses (a revoked token, a deleted database),
//! after 5 minutes. Any of them at once when the sync settings change.

use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use moul_sync::hub::EventParser;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::host::{HubEvents, WeakEngine};
use crate::js;

/// The hub pings every 15 seconds: three missed pings and the stream is
/// gone, though no error said so.
const SILENCE_MS: f64 = 45_000.0;
/// How often the loop looks up from a read or a wait, to stop once the
/// engine closes or the settings change.
const CHECK_MS: u32 = 1_000;
const FIRST_DELAY_MS: u32 = 1_000;
const MAX_DELAY_MS: u32 = 30_000;
/// A refusal will not change by asking again soon.
const REFUSED_DELAY_MS: u32 = 300_000;

/// How a stream ended.
enum Ended {
    /// The engine closed: stop.
    Gone,
    /// The sync settings changed: open the new stream, if any, at once.
    Changed,
    /// The stream was open, then dropped.
    Dropped,
    /// It could not be opened.
    Failed,
    /// The hub refused it.
    Refused,
}

/// Follows the hub's event stream while `engine` is open and syncs through
/// a hub, and runs a sync round on each `head`. Holds the engine only
/// between waits.
pub(crate) async fn follow(engine: WeakEngine) {
    let mut delay = FIRST_DELAY_MS;
    loop {
        let target = loop {
            let Some(e) = engine.upgrade() else { return };
            if e.closing() {
                return;
            }
            if let Some(t) = e.hub_events() {
                break t;
            }
            drop(e);
            js::sleep_ms(CHECK_MS).await;
        };
        let started = js_sys::Date::now();
        let ended = stream(&engine, &target).await;
        let lasted = js_sys::Date::now() - started;
        let wait = match ended {
            Ended::Gone => return,
            Ended::Changed => {
                delay = FIRST_DELAY_MS;
                continue;
            }
            Ended::Refused => REFUSED_DELAY_MS,
            // A stream that held a while and dropped is opened again soon;
            // one that keeps dropping at once backs off like a failure.
            Ended::Dropped if lasted >= f64::from(MAX_DELAY_MS) => {
                delay = FIRST_DELAY_MS;
                delay
            }
            Ended::Dropped | Ended::Failed => {
                let d = delay;
                delay = (delay * 2).min(MAX_DELAY_MS);
                d
            }
        };
        let mut waited = 0;
        while waited < wait {
            js::sleep_ms(CHECK_MS).await;
            waited += CHECK_MS;
            match check(&engine, &target) {
                Ok(()) => {}
                Err(Ended::Gone) => return,
                Err(_) => break,
            }
        }
    }
}

/// Whether the engine is open and still syncs through `target`.
fn check(engine: &WeakEngine, target: &HubEvents) -> Result<(), Ended> {
    let Some(e) = engine.upgrade() else {
        return Err(Ended::Gone);
    };
    if e.closing() {
        return Err(Ended::Gone);
    }
    if e.hub_events().as_ref() != Some(target) {
        return Err(Ended::Changed);
    }
    Ok(())
}

/// A promise that resolves with `value` after `ms`.
fn timer(ms: u32, value: &JsValue) -> Promise {
    let d = js::deferred();
    let resolve = d.resolve.bind1(&JsValue::NULL, value);
    let _ = js::call(&js::global(), "setTimeout", &[resolve.into(), ms.into()]);
    d.promise
}

/// Waits for `promise`, looking up every [`CHECK_MS`] to stop when the
/// engine closes, the settings change, or the stream has been silent since
/// `heard` for too long.
async fn settle(
    promise: &Promise,
    engine: &WeakEngine,
    target: &HubEvents,
    heard: f64,
) -> Result<JsValue, Ended> {
    let tick: JsValue = Object::new().into();
    loop {
        let race = Promise::race(&Array::of2(promise, &timer(CHECK_MS, &tick)));
        let v = JsFuture::from(race).await.map_err(|_| Ended::Failed)?;
        if !Object::is(&v, &tick) {
            return Ok(v);
        }
        check(engine, target)?;
        if js_sys::Date::now() - heard > SILENCE_MS {
            return Err(Ended::Dropped);
        }
    }
}

/// Opens the stream and reads it until it ends.
async fn stream(engine: &WeakEngine, target: &HubEvents) -> Ended {
    let Ok(controller) = js::get(&js::global(), "AbortController")
        .dyn_into::<Function>()
        .and_then(|c| Reflect::construct(&c, &Array::new()))
    else {
        return Ended::Gone; // no way to hold a stream here
    };
    let ended = read(engine, target, &controller).await;
    let _ = js::call(&controller, "abort", &[]);
    ended
}

async fn read(engine: &WeakEngine, target: &HubEvents, controller: &JsValue) -> Ended {
    let init = js::object(&[
        (
            "headers",
            js::object(&[
                ("Authorization", format!("Bearer {}", target.token).into()),
                ("Accept", "text/event-stream".into()),
            ]),
        ),
        ("signal", js::get(controller, "signal")),
        ("cache", "no-store".into()),
    ]);
    let Ok(fetching) = js::call(&js::global(), "fetch", &[target.url.as_str().into(), init])
        .and_then(|p| p.dyn_into::<Promise>())
    else {
        return Ended::Failed;
    };
    let mut heard = js_sys::Date::now();
    let response = match settle(&fetching, engine, target, heard).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    let status = js::get(&response, "status").as_f64().unwrap_or(0.0) as u16;
    match status {
        200 => {}
        401 | 403 | 404 => return Ended::Refused,
        _ => return Ended::Failed,
    }
    let body = js::get(&response, "body");
    let Ok(reader) = js::call(&body, "getReader", &[]) else {
        return Ended::Failed;
    };
    let mut parser = EventParser::new();
    loop {
        let Ok(next) = js::call(&reader, "read", &[]).and_then(|p| p.dyn_into::<Promise>()) else {
            return Ended::Dropped;
        };
        let chunk = match settle(&next, engine, target, heard).await {
            Ok(c) => c,
            Err(Ended::Failed) => return Ended::Dropped,
            Err(e) => return e,
        };
        if js::get(&chunk, "done").as_bool() == Some(true) {
            return Ended::Dropped;
        }
        heard = js_sys::Date::now();
        let Ok(bytes) = js::get(&chunk, "value").dyn_into::<Uint8Array>() else {
            continue;
        };
        for event in parser.push(&bytes.to_vec()) {
            if event.event != "head" {
                continue;
            }
            let Some(e) = engine.upgrade() else {
                return Ended::Gone;
            };
            // Read on while the round runs, so pings keep the stream alive.
            spawn_local(async move { e.on_head(&event.data).await });
        }
    }
}
