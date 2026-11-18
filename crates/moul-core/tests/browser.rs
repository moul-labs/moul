//! The engine in a real browser, run in Chromium, Firefox and WebKit by
//! `browser-tests/run.mjs`: definitions, records, rules, sign-in (bcrypt
//! and token signing in WebAssembly) and the change log, from a dedicated
//! worker as in the product.

#![cfg(target_arch = "wasm32")]

use moul_core::{Config, Engine, Request, SeededHost, StoreBlobs};
use moul_store::Store;
use serde_json::json;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_dedicated_worker);

#[wasm_bindgen_test]
fn the_engine_serves_requests_in_a_worker() {
    let mut e = Engine::open(
        Store::open_in_memory().unwrap(),
        SeededHost::new(5, 1_794_961_964_000),
        StoreBlobs,
        Config::default(),
    )
    .unwrap();
    let r = e.handle(Request::post("/api/moul", json!({"name": "users", "type": "auth"})).owner());
    assert_eq!(r.status, 201);
    let r = e.handle(
        Request::post(
            "/api/moul",
            json!({"name": "notes", "fields": [{"name": "text", "type": "text"}],
                   "rules": {"listRule": "@request.auth.id != ''", "createRule": "@request.auth.id != ''"}}),
        )
        .owner(),
    );
    assert_eq!(r.status, 201);
    let r = e.handle(Request::post(
        "/api/moul/users/records",
        json!({"username": "ana", "email": "ana@example.com", "password": "Password123!", "passwordConfirm": "Password123!"}),
    ));
    assert_eq!(r.status, 201, "{:?}", r.body);
    let r = e.handle(Request::post(
        "/api/moul/users/auth-with-password",
        json!({"identity": "ana", "password": "Password123!"}),
    ));
    assert_eq!(r.status, 200, "{:?}", r.body);
    let token = r.json_body()["token"].as_str().unwrap().to_owned();

    assert_eq!(
        e.handle(Request::post(
            "/api/moul/notes/records",
            json!({"text": "x"})
        ))
        .status,
        401
    );
    let r =
        e.handle(Request::post("/api/moul/notes/records", json!({"text": "hello"})).bearer(&token));
    assert_eq!(r.status, 201, "{:?}", r.body);
    let list = e.handle(Request::get("/api/moul/notes/records").bearer(&token));
    assert_eq!(list.json_body()["totalItems"], 1);
    assert_eq!(
        e.handle(Request::get("/api/moul/notes/records"))
            .json_body()["totalItems"],
        0
    );
    assert!(e.changes(0, 10).len() >= 4);
}

/// The browser's SQLite has `sqlite_dbpage`, which reads raw pages of any
/// attached store. Rule SQL must not reach it.
#[wasm_bindgen_test]
fn rule_sql_cannot_read_raw_pages_in_the_browser() {
    let mut e = Engine::open(
        Store::open_in_memory().unwrap(),
        SeededHost::new(6, 1_794_961_964_000),
        StoreBlobs,
        Config::default(),
    )
    .unwrap();
    let probe = "exists('notes', \"(SELECT count(*) FROM sqlite_dbpage('writer')) > 0\")";
    let r = e.handle(
        Request::post(
            "/api/moul",
            json!({"name": "notes", "fields": [{"name": "text", "type": "text"}],
                   "rules": {"viewRule": probe, "createRule": ""}}),
        )
        .owner(),
    );
    assert_eq!(r.status, 201);
    let note = e.handle(Request::post(
        "/api/moul/notes/records",
        json!({"text": "x"}),
    ));
    let id = note.json_body()["id"].as_str().unwrap().to_owned();
    let r = e.handle(Request::get(&format!("/api/moul/notes/records/{id}")));
    assert_eq!(r.status, 404, "the probe must see no pages");
    let r = e.handle(
        Request::patch(
            "/api/moul/notes",
            json!({"rules": {"viewRule": "exists('notes', '1 = 1')"}}),
        )
        .owner(),
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        e.handle(Request::get(&format!("/api/moul/notes/records/{id}")))
            .status,
        200
    );
}
