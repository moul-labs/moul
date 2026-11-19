//! The engine end to end, through requests, on an in-memory database.

#![cfg(not(target_arch = "wasm32"))]

use moul_core::{
    Action, Body, Config, Engine, MemoryBlobs, Request, Response, Role, SeededHost, Subscription,
    Upload,
};
use moul_store::Store;
use serde_json::{Value, json};

const NOW: i64 = 1_794_961_964_000; // 2026-11-18T00:32:44Z

fn engine() -> Engine {
    engine_with(Config::default())
}

fn engine_with(config: Config) -> Engine {
    Engine::open(
        Store::open_in_memory().unwrap(),
        SeededHost::new(1, NOW),
        MemoryBlobs::new(),
        config,
    )
    .unwrap()
}

fn ok(r: Response, status: u16) -> Value {
    assert_eq!(r.status, status, "{:?}", r.body);
    r.json_body().clone()
}

fn define(e: &mut Engine, body: Value) -> Value {
    ok(e.handle(Request::post("/api/moul", body).owner()), 201)
}

fn posts(e: &mut Engine) -> Value {
    define(
        e,
        json!({
            "name": "posts",
            "fields": [
                {"name": "title", "type": "text", "required": true},
                {"name": "views", "type": "number"},
                {"name": "published", "type": "bool"},
                {"name": "meta", "type": "json"},
                {"name": "status", "type": "select", "options": ["draft", "live"]},
            ],
        }),
    )
}

#[test]
fn collections_get_ids_and_fields_keep_theirs_through_renames() {
    let mut e = engine();
    let c = posts(&mut e);
    assert_eq!(c["name"], "posts");
    assert_eq!(c["type"], "base");
    assert_eq!(c["createdAt"], "2026-11-18T00:32:44Z");
    let title_id = c["fields"][0]["id"].as_str().unwrap().to_owned();
    assert!(title_id.starts_with("fld_"));

    let r = ok(
        e.handle(
            Request::post(
                "/api/moul/posts/records",
                json!({"title": "Hello", "views": "12.50"}),
            )
            .owner(),
        ),
        201,
    );
    let id = r["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("post-"), "{id}");
    assert_eq!(r["views"], json!(12.5));

    // Rename the collection and the field: same ids, data kept.
    let mut fields = c["fields"].clone();
    fields[0]["name"] = json!("headline");
    let c2 = ok(
        e.handle(
            Request::patch(
                "/api/moul/posts",
                json!({"name": "articles", "fields": fields}),
            )
            .owner(),
        ),
        200,
    );
    assert_eq!(c2["id"], c["id"]);
    assert_eq!(c2["fields"][0]["id"], json!(title_id));
    let got = ok(
        e.handle(Request::get(&format!("/api/moul/articles/records/{id}")).owner()),
        200,
    );
    assert_eq!(got["headline"], "Hello");
    assert_eq!(
        e.handle(Request::get("/api/moul/posts").owner()).status,
        404
    );
}

#[test]
fn definition_errors_match_go() {
    let mut e = engine();
    let r = e.handle(Request::post("/api/moul", json!({"name": "_x"})).owner());
    assert_eq!(
        (r.status, r.message()),
        (400, "Moul name cannot start with underscore")
    );
    let r = e.handle(Request::post("/api/moul", json!({"name": "posts"})));
    assert_eq!(
        (r.status, r.message()),
        (
            401,
            "Admin authentication required (root user token or admin key)"
        )
    );
    posts(&mut e);
    let r = e.handle(Request::post("/api/moul", json!({"name": "posts"})).owner());
    assert_eq!(
        (r.status, r.message()),
        (400, "Moul with name \"posts\" already exists")
    );
    let r = e.handle(Request::patch("/api/moul/posts", json!({"type": "auth"})).owner());
    assert_eq!(
        (r.status, r.message()),
        (
            400,
            "cannot change collection type from \"base\" to \"auth\""
        )
    );
}

#[test]
fn records_validate_like_go() {
    let mut e = engine();
    posts(&mut e);
    let cases = [
        (json!({}), "Field \"title\" is required"),
        (
            json!({"title": "  "}),
            "Field \"title\" is required and cannot be empty",
        ),
        (
            json!({"title": "a", "views": "0x10"}),
            "Field \"views\" must be a number",
        ),
        // Go's ParseFloat reads "1_000", so the stricter check answers.
        (
            json!({"title": "a", "views": "1_000"}),
            "Field \"views\" must be a decimal number",
        ),
        (
            json!({"title": "a", "published": "maybe"}),
            "Field \"published\" must be a boolean",
        ),
        (
            json!({"title": "a", "status": "gone"}),
            "Field \"status\" must be one of: draft, live",
        ),
    ];
    for (body, msg) in cases {
        let r = e.handle(Request::post("/api/moul/posts/records", body.clone()).owner());
        assert_eq!((r.status, r.message()), (400, msg), "{body}");
    }
    let r = ok(
        e.handle(
            Request::post(
                "/api/moul/posts/records",
                json!({"title": "a", "published": "true", "meta": {"k": [1, 2]}, "status": "live"}),
            )
            .owner(),
        ),
        201,
    );
    assert_eq!(r["published"], true);
    assert_eq!(r["meta"], json!({"k": [1, 2]}));
}

#[test]
fn list_pages_and_counts() {
    let mut e = engine();
    posts(&mut e);
    for i in 0..7 {
        e.handle(
            Request::post(
                "/api/moul/posts/records",
                json!({"id": format!("p{i}"), "title": format!("t{i}")}),
            )
            .owner(),
        );
    }
    let page = ok(
        e.handle(
            Request::get("/api/moul/posts/records")
                .query("perPage", "3")
                .query("page", "2")
                .owner(),
        ),
        200,
    );
    assert_eq!(page["totalItems"], 7);
    assert_eq!(page["totalPages"], 3);
    assert_eq!(page["page"], 2);
    let ids: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["p3", "p4", "p5"]);
    let after = ok(
        e.handle(
            Request::get("/api/moul/posts/records")
                .query("perPage", "2")
                .query("after", "p4")
                .owner(),
        ),
        200,
    );
    let ids: Vec<&str> = after["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["p5", "p6"]);
    let found = ok(
        e.handle(
            Request::get("/api/moul/posts/records")
                .query("search", "t6")
                .owner(),
        ),
        200,
    );
    assert_eq!(found["totalItems"], 1);
}

#[test]
fn writes_are_named_requests_in_the_change_log() {
    let mut e = engine();
    posts(&mut e);
    let req = Request::post("/api/moul/posts/records", json!({"title": "once"}))
        .owner()
        .with_id("phone:41");
    let first = e.handle(req.clone());
    assert_eq!(first.status, 201);
    assert_eq!(first.header_value("Moul-Request-Id"), Some("phone:41"));
    // The same request again is answered from the log and applied once.
    let again = e.handle(req);
    assert_eq!(again.status, 201);
    assert_eq!(again.header_value("Moul-Replayed"), Some("true"));
    assert_eq!(again.json_body()["id"], first.json_body()["id"]);
    let all = ok(
        e.handle(Request::get("/api/moul/posts/records").owner()),
        200,
    );
    assert_eq!(all["totalItems"], 1);

    // A refused write is logged as refused, with the reason.
    let refused = e.handle(
        Request::post("/api/moul/posts/records", json!({}))
            .owner()
            .with_id("phone:42"),
    );
    assert_eq!(refused.status, 400);

    let log = e.changes(0, 100);
    let ops: Vec<(&str, &str, u16)> = log
        .iter()
        .map(|c| (c.request.as_str(), c.op.as_str(), c.status))
        .collect();
    assert_eq!(
        ops[1..],
        [("phone:41", "create", 201), ("phone:42", "create", 400)]
    );
    assert_eq!(log[2].error.as_deref(), Some("Field \"title\" is required"));
    assert_eq!(log[1].actor, "owner");
    // The record's fields are listed by field id.
    assert!(log[1].fields.iter().any(|f| f.starts_with("fld_")));
    // A write without an id gets this device's next one.
    assert!(
        log[0].request.starts_with(&format!("{}:", e.device_id())),
        "{}",
        log[0].request
    );
}

#[test]
fn auth_with_password_and_rules_on_owner() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "users", "type": "auth", "rules": {"createRule": ""}}),
    );
    let r = e.handle(Request::post(
        "/api/moul/users/records",
        json!({"username": "alice", "email": "alice@example.com", "password": "Password123!", "passwordConfirm": "Password123!"}),
    ));
    let user = ok(r, 201);
    // Guests do not see the email of a record that is not theirs.
    assert!(user.get("email").is_none(), "{user}");
    assert!(user.get("passwordHash").is_none());

    let bad = e.handle(Request::post(
        "/api/moul/users/auth-with-password",
        json!({"identity": "alice", "password": "nope"}),
    ));
    assert_eq!((bad.status, bad.message()), (400, "Invalid credentials"));
    let signed = ok(
        e.handle(Request::post(
            "/api/moul/users/auth-with-password",
            json!({"identity": "alice@example.com", "password": "Password123!"}),
        )),
        200,
    );
    assert_eq!(signed["record"]["email"], "alice@example.com");
    let token = signed["token"].as_str().unwrap().to_owned();

    // The shared store holds no credentials; the writer-only store does.
    let conn = e.store().conn();
    let shared: i64 = conn
        .query_row(
            "SELECT count(*) FROM pragma_table_info('users') WHERE name LIKE '%password%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(shared, 0);
    let hash: String = conn
        .query_row("SELECT passwordHash FROM writer._auth", [], |r| r.get(0))
        .unwrap();
    assert!(hash.starts_with("$2a$10$"));

    // Refresh exchanges the token once; logout revokes the new one.
    let refreshed = ok(
        e.handle(Request::new("POST", "/api/moul/users/refresh").bearer(&token)),
        200,
    );
    let again = e.handle(Request::new("POST", "/api/moul/users/refresh").bearer(&token));
    assert_eq!(
        (again.status, again.message()),
        (401, "Token has been revoked")
    );
    let new_token = refreshed["token"].as_str().unwrap();
    ok(
        e.handle(Request::new("POST", "/api/moul/users/logout").bearer(new_token)),
        200,
    );
    let id = user["id"].as_str().unwrap();
    let r = e.handle(Request::get(&format!("/api/moul/users/records/{id}")).bearer(new_token));
    assert_eq!(
        (r.status, r.message()),
        (401, "Authentication required to view this record")
    );
}

#[test]
fn email_codes_land_in_the_local_inbox() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    ok(
        e.handle(Request::post(
            "/api/moul/users/otp/request",
            json!({"email": "bo@example.com"}),
        )),
        200,
    );
    let inbox = ok(
        e.handle(
            Request::get("/api/inbox")
                .query("to", "bo@example.com")
                .owner(),
        ),
        200,
    );
    let mail = &inbox["items"][0];
    assert_eq!(mail["subject"], "Your OTP Code");
    let body = mail["body"].as_str().unwrap();
    let code: String = body
        .split("is:\n\n")
        .nth(1)
        .unwrap()
        .chars()
        .take(6)
        .collect();
    assert!(code.chars().all(|c| c.is_ascii_digit()), "{body}");

    // The unknown email signed up: a record exists, logged as a create.
    assert!(
        e.changes(0, 10)
            .iter()
            .any(|c| c.op == "create" && c.status == 200)
    );

    let wrong = e.handle(Request::post(
        "/api/moul/users/auth-with-otp",
        json!({"email": "bo@example.com", "code": "000000x"}),
    ));
    assert_eq!(wrong.message(), "Invalid email or OTP code");
    let signed = ok(
        e.handle(Request::post(
            "/api/moul/users/auth-with-otp",
            json!({"email": "bo@example.com", "code": code}),
        )),
        200,
    );
    assert_eq!(signed["record"]["username"], "bo");
    // A code works once.
    let reuse = e.handle(Request::post(
        "/api/moul/users/auth-with-otp",
        json!({"email": "bo@example.com", "code": code}),
    ));
    assert_eq!(
        reuse.message(),
        "No active OTP found for this email. Please request a new one."
    );
}

#[test]
fn otp_guesses_are_bounded() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    ok(
        e.handle(Request::post(
            "/api/moul/users/otp/request",
            json!({"email": "c@example.com"}),
        )),
        200,
    );
    for _ in 0..5 {
        let r = e.handle(Request::post(
            "/api/moul/users/auth-with-otp",
            json!({"email": "c@example.com", "code": "x"}),
        ));
        assert_eq!(r.status, 400);
    }
    let r = e.handle(Request::post(
        "/api/moul/users/auth-with-otp",
        json!({"email": "c@example.com", "code": "x"}),
    ));
    assert_eq!(r.status, 429);
    assert_eq!(r.json_body()["reason"], "code");
}

#[test]
fn password_reset_through_the_inbox() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    ok(
        e.handle(Request::post(
            "/api/moul/users/records",
            json!({"username": "d", "email": "d@example.com", "password": "Password123!", "passwordConfirm": "Password123!"}),
        )),
        201,
    );
    ok(
        e.handle(Request::post(
            "/api/moul/users/request-password-reset",
            json!({"email": "d@example.com"}),
        )),
        200,
    );
    let inbox = ok(e.handle(Request::get("/api/inbox").owner()), 200);
    let body = inbox["items"][0]["body"].as_str().unwrap();
    let token = body
        .split("token=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    ok(
        e.handle(Request::post(
            "/api/moul/users/confirm-password-reset",
            json!({"token": token, "password": "NewPassword9", "passwordConfirm": "NewPassword9"}),
        )),
        200,
    );
    ok(
        e.handle(Request::post(
            "/api/moul/users/auth-with-password",
            json!({"identity": "d", "password": "NewPassword9"}),
        )),
        200,
    );
}

#[test]
fn budgets_stop_big_requests() {
    let mut config = Config::default();
    config.budget.rows = 3;
    let mut e = engine_with(config);
    posts(&mut e);
    for i in 0..5 {
        e.handle(
            Request::post("/api/moul/posts/records", json!({"title": format!("t{i}")})).owner(),
        );
    }
    let r = e.handle(Request::get("/api/moul/posts/records").owner());
    assert_eq!(r.status, 400);
    assert_eq!(r.json_body()["budget"], "rows");
    // A request may ask for less than the engine allows, never more.
    let r = e.handle(
        Request::get("/api/moul/posts/records")
            .query("perPage", "2")
            .header("Moul-Budget", "rows=1")
            .owner(),
    );
    assert_eq!(r.json_body()["budget"], "rows");
    let r = e.handle(
        Request::get("/api/moul/posts/records")
            .query("perPage", "2")
            .header("Moul-Budget", "bytes=10")
            .owner(),
    );
    assert_eq!(r.json_body()["budget"], "bytes");
    // SQLite steps are capped too: a long statement stops.
    let mut e = engine();
    posts(&mut e);
    let big: Vec<Value> = (0..2000)
        .map(|i| json!({"title": format!("t{i}")}))
        .collect();
    ok(
        e.handle(Request::post("/api/moul/posts/import", json!(big)).owner()),
        200,
    );
    let r = e.handle(
        Request::get("/api/moul/posts/records")
            .query("search", "t1")
            .header("Moul-Budget", "ksteps=2")
            .owner(),
    );
    assert_eq!(r.json_body()["budget"], "steps", "{:?}", r.body);
    // Without the lower budget the same search runs. Titles t1, t1x, t1xx and
    // t1xxx match, and so may a random id.
    let found = ok(
        e.handle(
            Request::get("/api/moul/posts/records")
                .query("search", "t1")
                .owner(),
        ),
        200,
    );
    assert!(found["totalItems"].as_i64().unwrap() >= 1111);
}

#[test]
fn uploads_are_stored_once_and_served_with_access_checks() {
    // Bytes kept in the device store, as in the browser.
    let mut e = Engine::open(
        Store::open_in_memory().unwrap(),
        SeededHost::new(1, NOW),
        moul_core::StoreBlobs,
        Config::default(),
    )
    .unwrap();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    define(
        &mut e,
        json!({"name": "docs", "fields": [{"name": "file", "type": "file"}], "rules": {"viewRule": "@request.auth.id != ''"}}),
    );
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01rest".to_vec();
    let upload = |name: &str, data: Vec<u8>| {
        let mut r = Request::new("POST", "/api/upload").owner();
        r.body = Body::Form {
            fields: vec![],
            files: vec![(
                "file".into(),
                Upload {
                    name: name.into(),
                    content_type: "image/png".into(),
                    data,
                },
            )],
        };
        r
    };
    let info = ok(e.handle(upload("My Photo.PNG", png.clone())), 200);
    let url = info[0]["url"].as_str().unwrap().to_owned();
    assert!(url.contains("/original.png?sig="), "{url}");
    assert_eq!(info[0]["filename"], "my-photo.png");
    let served = e.handle(Request::from_target("GET", &url));
    assert_eq!(served.status, 200);
    assert_eq!(
        served.header_value("Content-Security-Policy"),
        Some(moul_core::files::STORAGE_CSP)
    );
    // Without the signature, a guest cannot read a file no public record holds.
    let bare = url.split('?').next().unwrap();
    assert_eq!(e.handle(Request::get(bare)).status, 404);

    // The same bytes again are stored once.
    ok(e.handle(upload("copy.png", png.clone())), 200);
    let blobs: i64 = e
        .store()
        .conn()
        .query_row("SELECT count(*) FROM device._blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(blobs, 1);
    // A member may not upload active content.
    let mut html = Request::new("POST", "/api/upload").owner();
    html.caller = Default::default();
    assert_eq!(e.handle(html).status, 401);
}

#[test]
fn import_and_export_round_trip() {
    let mut e = engine();
    posts(&mut e);
    let csv = "id,title,views,published\np1,One,1,true\np2,\"Two, with comma\",2.5,false\n";
    let mut req = Request::new("POST", "/api/moul/posts/import").owner();
    req.body = Body::Bytes {
        content_type: "text/csv".into(),
        data: csv.as_bytes().to_vec(),
    };
    let res = ok(e.handle(req), 200);
    assert_eq!(
        (res["inserted"].as_i64(), res["success"].as_bool()),
        (Some(2), Some(true))
    );
    let out = e.handle(
        Request::get("/api/moul/posts/export")
            .query("format", "csv")
            .query("sort", "")
            .owner(),
    );
    assert_eq!(out.status, 200);
    let moul_core::ResponseBody::Bytes { data, .. } = &out.body else {
        panic!()
    };
    let text = String::from_utf8(data.clone()).unwrap();
    assert!(
        text.starts_with("id,title,views,published,meta,status,createdAt,updatedAt\n"),
        "{text}"
    );
    assert!(text.contains("p2,\"Two, with comma\",2.5,false"), "{text}");

    // Atomic import: one bad row and nothing lands.
    let bad = json!([{"id": "p3", "title": "ok"}, {"id": "p4"}]);
    let r = e.handle(Request::post("/api/moul/posts/import", bad).owner());
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json_body()["error"],
        "row 2 validation failed: field \"title\" is required"
    );
    let all = ok(
        e.handle(Request::get("/api/moul/posts/records").owner()),
        200,
    );
    assert_eq!(all["totalItems"], 2);
}

#[test]
fn schema_file_applies_as_definition_requests() {
    let mut e = engine();
    let file = json!({
        "format": 1,
        "collections": [
            {"name": "teams", "fields": [{"name": "name", "type": "text"}]},
            {"name": "members", "fields": [
                {"name": "team", "type": "relation", "relationConfig": {"targetMoul": "teams", "cardinality": "1:N"}},
            ]},
        ],
    });
    let applied = ok(
        e.handle(Request::post("/api/schema", file.clone()).owner()),
        200,
    );
    assert_eq!(applied["created"], json!(["teams", "members"]));
    // Applying the exported file changes nothing.
    let exported = ok(e.handle(Request::get("/api/schema").owner()), 200);
    let again = ok(
        e.handle(Request::post("/api/schema", exported.clone()).owner()),
        200,
    );
    assert_eq!(again["unchanged"], json!(["members", "teams"]));
    // A rename in the file, by field id, keeps the column's data.
    let mut renamed = exported.clone();
    renamed["collections"][1]["fields"][0]["name"] = json!("title");
    let r = ok(e.handle(Request::post("/api/schema", renamed).owner()), 200);
    assert_eq!(r["updated"], json!(["teams"]));

    let types = e.handle(Request::get("/api/types").owner());
    let moul_core::ResponseBody::Bytes { data, .. } = &types.body else {
        panic!()
    };
    let ts = String::from_utf8(data.clone()).unwrap();
    assert!(ts.contains("export interface MembersRecord extends BaseSystemFields {\n  team?: string;\n  expand?: {\n    team?: TeamsRecord | null;\n  };\n}"), "{ts}");
    assert!(ts.contains("\"teams\": TeamsRecord;"));
}

#[test]
fn typegen_types_an_unset_bool_as_null() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "flags", "fields": [
            {"name": "done", "type": "bool"},
            {"name": "seen", "type": "bool", "required": true},
        ]}),
    );
    let made = ok(
        e.handle(Request::post("/api/moul/flags/records", json!({"seen": true})).owner()),
        201,
    );
    assert_eq!(made["done"], Value::Null, "an unset bool reads as null");
    let types = e.handle(Request::get("/api/types").owner());
    let moul_core::ResponseBody::Bytes { data, .. } = &types.body else {
        panic!()
    };
    let ts = String::from_utf8(data.clone()).unwrap();
    assert!(
        ts.contains("  done: boolean | null;\n  seen: boolean;\n"),
        "{ts}"
    );
}

#[test]
fn live_subscriptions_receive_changes() {
    let mut e = engine();
    posts(&mut e);
    let sub = e
        .subscribe(Subscription {
            collection: "posts".into(),
            ..Default::default()
        })
        .unwrap();
    let created = ok(
        e.handle(Request::post("/api/moul/posts/records", json!({"title": "live"})).owner()),
        201,
    );
    let id = created["id"].as_str().unwrap();
    e.handle(
        Request::patch(
            &format!("/api/moul/posts/records/{id}"),
            json!({"title": "edited"}),
        )
        .owner(),
    );
    e.handle(Request::delete(&format!("/api/moul/posts/records/{id}")).owner());
    let got: Vec<(Action, String)> = e
        .take_deliveries()
        .into_iter()
        .map(|d| {
            assert_eq!(d.subscription, sub);
            (
                d.event.action,
                d.event.record["title"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (Action::Create, "live".into()),
            (Action::Update, "edited".into()),
            (Action::Delete, "edited".into())
        ]
    );
}

#[test]
fn followers_refuse_writes_and_catch_up_from_the_log() {
    let mut e = engine();
    posts(&mut e);
    e.handle(Request::post("/api/moul/posts/records", json!({"title": "a"})).owner());
    let store = e.into_store();
    let mut f = Engine::open_follower(
        store,
        SeededHost::new(2, NOW),
        MemoryBlobs::new(),
        Config::default(),
    )
    .unwrap();
    assert_eq!(f.role(), Role::Follower);
    let r = f.handle(Request::post("/api/moul/posts/records", json!({"title": "b"})).owner());
    assert_eq!(r.status, 409);
    assert_eq!(
        ok(
            f.handle(Request::get("/api/moul/posts/records").owner()),
            200
        )["totalItems"],
        1
    );
    f.subscribe(Subscription {
        collection: "posts".into(),
        owner: true,
        ..Default::default()
    })
    .unwrap();
    // A follower starts at the newest change; replay from the start.
    assert_eq!(f.catch_up().len(), 0);
    f.set_seen(0);
    let caught = f.catch_up();
    assert_eq!(caught.len(), 2);
    assert_eq!(f.take_deliveries().len(), 1);
}

#[test]
fn a_go_database_opens_and_its_users_sign_in() {
    let store = Store::open_in_memory().unwrap();
    moul_fixtures::Fixtures::default()
        .db("cbc2a77b5c6e0079")
        .unwrap()
        .load_into(store.conn())
        .unwrap();
    let mut e = Engine::open(
        store,
        SeededHost::new(9, NOW),
        MemoryBlobs::new(),
        Config::default(),
    )
    .unwrap();
    let signed = ok(
        e.handle(Request::post(
            "/api/moul/users/auth-with-password",
            json!({"identity": "admin@example.com", "password": "Password123!"}),
        )),
        200,
    );
    assert_eq!(signed["record"]["id"], "usr_admin_001");
    let list = ok(
        e.handle(Request::get("/api/moul/categories/records").owner()),
        200,
    );
    assert_eq!(list["totalItems"], 4);
}

fn sign_up(e: &mut Engine, name: &str) -> (String, String) {
    let user = ok(
        e.handle(Request::post(
            "/api/moul/users/records",
            json!({"username": name, "email": format!("{name}@example.com"),
                   "password": "Password123!", "passwordConfirm": "Password123!"}),
        )),
        201,
    );
    let signed = ok(
        e.handle(Request::post(
            "/api/moul/users/auth-with-password",
            json!({"identity": name, "password": "Password123!"}),
        )),
        200,
    );
    (
        user["id"].as_str().unwrap().to_owned(),
        signed["token"].as_str().unwrap().to_owned(),
    )
}

#[test]
fn rules_decide_who_sees_what() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    define(
        &mut e,
        json!({
            "name": "notes",
            "fields": [
                {"name": "text", "type": "text"},
                {"name": "owner", "type": "relation", "relationConfig": {"targetMoul": "users", "cardinality": "1:N"}},
                {"name": "shared", "type": "bool"},
            ],
            "rules": {
                "listRule": "owner = @request.auth.id || shared = true",
                "viewRule": "owner = @request.auth.id || shared = true",
                "createRule": "@request.auth.id != '' && owner = @request.auth.id",
                "updateRule": "owner = @request.auth.id",
                "deleteRule": "owner = @request.auth.id",
            },
        }),
    );
    let (alice, alice_token) = sign_up(&mut e, "alice");
    let (bob, bob_token) = sign_up(&mut e, "bob");
    let bob_sub = e
        .subscribe(Subscription {
            collection: "notes".into(),
            credentials: vec![("Authorization".into(), format!("Bearer {bob_token}"))],
            ..Default::default()
        })
        .unwrap();

    let post = |e: &mut Engine, token: &str, body: Value| {
        e.handle(Request::post("/api/moul/notes/records", body).bearer(token))
    };
    // Guests cannot create; nobody can create for someone else.
    let r = e.handle(Request::post(
        "/api/moul/notes/records",
        json!({"text": "x", "owner": alice}),
    ));
    assert_eq!(r.status, 401);
    assert_eq!(
        post(&mut e, &bob_token, json!({"text": "x", "owner": alice})).status,
        403
    );
    let private = ok(
        post(
            &mut e,
            &alice_token,
            json!({"text": "mine", "owner": alice}),
        ),
        201,
    );
    ok(
        post(
            &mut e,
            &alice_token,
            json!({"text": "ours", "owner": alice, "shared": true}),
        ),
        201,
    );
    ok(
        post(&mut e, &bob_token, json!({"text": "bob's", "owner": bob})),
        201,
    );

    let list = |e: &mut Engine, token: &str, filter: &str| -> Vec<String> {
        let mut r = Request::get("/api/moul/notes/records")
            .bearer(token)
            .query("sort", "text");
        if !filter.is_empty() {
            r = r.query("filter", filter);
        }
        ok(e.handle(r), 200)["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["text"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(list(&mut e, &alice_token, ""), ["mine", "ours"]);
    assert_eq!(list(&mut e, &bob_token, ""), ["bob's", "ours"]);
    // A filter narrows what the rule allows and never widens it.
    assert_eq!(list(&mut e, &bob_token, "text ~ 'm'"), Vec::<String>::new());
    assert_eq!(
        list(&mut e, &bob_token, "text = 'mine' || 1 = 1"),
        ["bob's", "ours"]
    );

    // A record bob may not view answers like a missing one.
    let id = private["id"].as_str().unwrap();
    let r = e.handle(Request::get(&format!("/api/moul/notes/records/{id}")).bearer(&bob_token));
    assert_eq!((r.status, r.message()), (404, "Record not found"));
    // Expanding a relation honours the target's view rule: bob sees only himself.
    let mine = ok(
        e.handle(
            Request::get("/api/moul/notes/records")
                .bearer(&bob_token)
                .query("filter", "text = 'ours'")
                .query("expand", "owner"),
        ),
        200,
    );
    assert_eq!(mine["items"][0]["expand"]["owner"], Value::Null);

    // Bob got live events only for what he may see: his own note and the
    // shared one. Making the shared note private reaches him as a delete.
    let shared_id = list(&mut e, &alice_token, "shared = true");
    assert_eq!(shared_id, ["ours"]);
    let ours = ok(
        e.handle(
            Request::get("/api/moul/notes/records")
                .bearer(&alice_token)
                .query("filter", "shared = true"),
        ),
        200,
    )["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(
        e.handle(
            Request::patch(
                &format!("/api/moul/notes/records/{ours}"),
                json!({"shared": false}),
            )
            .bearer(&alice_token),
        ),
        200,
    );
    let seen: Vec<(Action, String)> = e
        .take_deliveries()
        .into_iter()
        .filter(|d| d.subscription == bob_sub)
        .map(|d| {
            (
                d.event.action,
                d.event.record["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (Action::Create, "ours".into()),
            (Action::Create, "bob's".into()),
            (Action::Delete, "ours".into()),
        ]
    );
}

#[test]
fn rule_sql_cannot_read_the_writer_or_device_store() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    ok(
        e.handle(Request::post(
            "/api/moul/users/records",
            json!({"username": "ann", "email": "ann@example.com",
                   "password": "Password123!", "passwordConfirm": "Password123!"}),
        )),
        201,
    );
    // A rule that would let anyone in exactly when a password hash starts
    // with "$2a$": the probe a leaking engine would answer.
    let probe = "exists('notes', \"(SELECT count(*) FROM writer._auth WHERE passwordHash LIKE '$2a$%') > 0\")";
    let plain = "exists('notes', 'text = ?', text)";
    define(
        &mut e,
        json!({"name": "notes", "fields": [{"name": "text", "type": "text"}],
               "rules": {"viewRule": probe, "listRule": "", "createRule": ""}}),
    );
    let note = ok(
        e.handle(Request::post(
            "/api/moul/notes/records",
            json!({"text": "hi"}),
        )),
        201,
    );
    let id = note["id"].as_str().unwrap();
    let r = e.handle(Request::get(&format!("/api/moul/notes/records/{id}")));
    assert_eq!(r.status, 404, "the probe must see no rows");
    // The same kind of rule over the shared store works.
    ok(
        e.handle(Request::patch("/api/moul/notes", json!({"rules": {"viewRule": plain}})).owner()),
        200,
    );
    ok(
        e.handle(Request::get(&format!("/api/moul/notes/records/{id}"))),
        200,
    );
    // A list rule naming a device-store table is refused when saved, and
    // fails closed in a database that holds one anyway.
    let naming = "@collection.moul_sync.key != ''";
    ok(
        e.handle(Request::patch("/api/moul/notes", json!({"rules": {"listRule": naming}})).owner()),
        400,
    );
    e.store()
        .conn()
        .execute(
            "UPDATE _moul SET rules = json_set(rules, '$.listRule', ?1) WHERE name = 'notes'",
            [naming],
        )
        .unwrap();
    e.store()
        .conn()
        .execute_batch("CREATE TABLE IF NOT EXISTS device.moul_sync (key TEXT, value TEXT); INSERT INTO device.moul_sync VALUES ('config', 'secret')")
        .unwrap();
    let r = e.handle(Request::get("/api/moul/notes/records"));
    assert_ne!(r.status, 200, "{:?}", r.body);
}

// ---------------------------------------------------------------- review fixes

#[test]
fn request_ids_cannot_be_claimed_or_hijacked() {
    let mut e = engine();
    posts(&mut e);
    let device = e.device_id().to_owned();
    // A guest may not name this device's own ids.
    let r = e.handle(
        Request::post("/api/moul/posts/records", json!({"title": "x"}))
            .header("Moul-Request-Id", &format!("{device}:99")),
    );
    assert_eq!(r.status, 400, "{:?}", r.body);
    // Even if such an id got into the log, the engine skips it.
    e.store()
        .conn()
        .execute(
            "INSERT INTO main._changes (request, actor, op, status, at) VALUES (?1, '', 'create', 401, 'x')",
            [format!("{device}:3")],
        )
        .unwrap();
    for i in 0..3 {
        ok(
            e.handle(
                Request::post("/api/moul/posts/records", json!({"title": format!("t{i}")})).owner(),
            ),
            201,
        );
    }
    // Guests are all one actor: they may not name request ids at all.
    let r = e.handle(
        Request::post("/api/moul/posts/records", json!({"title": "guest"})).with_id("phone:7"),
    );
    assert_eq!(r.status, 401);
    // A client id reused by someone else is a conflict, not a replay.
    define(&mut e, json!({"name": "users", "type": "auth"}));
    let (_, token) = sign_up(&mut e, "mia");
    let r = e.handle(
        Request::post("/api/moul/posts/records", json!({"title": "mia"}))
            .bearer(&token)
            .with_id("phone:7"),
    );
    assert_eq!(r.status, 201);
    let r = e.handle(
        Request::post("/api/moul/posts/records", json!({"title": "owner"}))
            .owner()
            .with_id("phone:7"),
    );
    assert_eq!(r.status, 409, "{:?}", r.body);
    // The same sender resending gets the original answer.
    let first = e.handle(
        Request::post("/api/moul/posts/records", json!({"title": "a"}))
            .owner()
            .with_id("phone:8"),
    );
    let again = e.handle(
        Request::post("/api/moul/posts/records", json!({"title": "a"}))
            .owner()
            .with_id("phone:8"),
    );
    assert_eq!(again.status, 201);
    assert_eq!(again.json_body()["id"], first.json_body()["id"]);
}

#[test]
fn a_write_only_collection_replays_its_create() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    define(
        &mut e,
        json!({"name": "inbox", "fields": [{"name": "text", "type": "text"}],
               "rules": {"createRule": "", "viewRule": "1 = 2"}}),
    );
    let (_, token) = sign_up(&mut e, "kiosk");
    let req = Request::post("/api/moul/inbox/records", json!({"text": "hi"}))
        .bearer(&token)
        .with_id("kiosk:1");
    let first = ok(e.handle(req.clone()), 201);
    // The sender cannot view the record: the replay says only that the
    // write was applied, and with which id. Expanding reaches nothing.
    let again = e.handle(req.query("expand", "user"));
    assert_eq!(again.status, 201, "{:?}", again.body);
    assert_eq!(again.json_body()["id"], first["id"]);
    assert_eq!(again.json_body()["replayed"], true);
    assert!(again.json_body().get("text").is_none());
}

#[test]
fn a_spent_budget_never_lets_a_rule_through() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "items", "fields": [{"name": "n", "type": "number"}]}),
    );
    let rows: Vec<Value> = (0..3000).map(|i| json!({"n": i})).collect();
    ok(
        e.handle(Request::post("/api/moul/items/import", json!(rows)).owner()),
        200,
    );
    define(
        &mut e,
        json!({"name": "notes", "fields": [{"name": "text", "type": "text"}],
               "rules": {"viewRule": "exists('items', 'n = ?', -1) == false", "createRule": ""}}),
    );
    let note = ok(
        e.handle(Request::post(
            "/api/moul/notes/records",
            json!({"text": "x"}),
        )),
        201,
    );
    let id = note["id"].as_str().unwrap();
    // Allowed with the full budget (no item has n = -1)...
    ok(
        e.handle(Request::get(&format!("/api/moul/notes/records/{id}"))),
        200,
    );
    // ...and a starved lookup is a budget error, never a pass.
    let r = e.handle(
        Request::get(&format!("/api/moul/notes/records/{id}")).header("Moul-Budget", "ksteps=1"),
    );
    assert_eq!(r.status, 400, "{:?}", r.body);
    assert_eq!(r.json_body()["budget"], "steps");
}

#[test]
fn an_answer_over_the_bytes_budget_commits_nothing() {
    let mut e = engine();
    posts(&mut e);
    let r = e.handle(
        Request::post("/api/moul/posts/records", json!({"title": "x".repeat(500)}))
            .owner()
            .header("Moul-Budget", "bytes=100"),
    );
    assert_eq!(r.json_body()["budget"], "bytes");
    assert_eq!(
        ok(
            e.handle(Request::get("/api/moul/posts/records").owner()),
            200
        )["totalItems"],
        0
    );
}

#[test]
fn live_events_follow_go_rules_tokens_and_signing() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    define(
        &mut e,
        json!({"name": "docs", "fields": [{"name": "text", "type": "text"}, {"name": "file", "type": "file"}],
               "rules": {"listRule": "@request.auth.id != ''", "viewRule": "", "createRule": ""}}),
    );
    let (_, token) = sign_up(&mut e, "ana");
    let guest = e
        .subscribe(Subscription {
            collection: "docs".into(),
            ..Default::default()
        })
        .unwrap();
    let member = e
        .subscribe(Subscription {
            collection: "docs".into(),
            credentials: vec![("Authorization".into(), format!("Bearer {token}"))],
            ..Default::default()
        })
        .unwrap();
    ok(
        e.handle(Request::post(
            "/api/moul/docs/records",
            json!({"text": "a"}),
        )),
        201,
    );
    let got: Vec<_> = e
        .take_deliveries()
        .into_iter()
        .map(|d| d.subscription)
        .collect();
    // The list rule guards live events when the view rule is public.
    assert_eq!(got, [member]);
    let _ = guest;

    // Logging out ends the subscription.
    ok(
        e.handle(Request::new("POST", "/api/moul/users/logout").bearer(&token)),
        200,
    );
    ok(
        e.handle(Request::post(
            "/api/moul/docs/records",
            json!({"text": "b"}),
        )),
        201,
    );
    assert!(e.take_deliveries().is_empty());
    let closed = e.take_closed();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].0, member);
}

#[test]
fn a_host_can_end_every_subscription() {
    let mut e = engine();
    posts(&mut e);
    let sub = |e: &mut Engine| {
        e.subscribe(Subscription {
            collection: "posts".into(),
            owner: true,
            ..Default::default()
        })
        .unwrap()
    };
    let (a, b) = (sub(&mut e), sub(&mut e));
    e.end_subscriptions("replaced");
    let mut closed = e.take_closed();
    closed.sort();
    assert_eq!(closed, [(a, "replaced".into()), (b, "replaced".into())]);
    ok(
        e.handle(Request::post("/api/moul/posts/records", json!({"title": "after"})).owner()),
        201,
    );
    assert!(
        e.take_deliveries().is_empty(),
        "ended subscriptions get nothing"
    );
    assert!(!e.unsubscribe(a));
}

#[test]
fn live_file_urls_are_signed_like_responses() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "docs", "fields": [{"name": "file", "type": "file"}],
               "rules": {"viewRule": "1 = 2"}}),
    );
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01rest".to_vec();
    let mut up = Request::new("POST", "/api/upload").owner();
    up.body = Body::Form {
        fields: vec![],
        files: vec![(
            "file".into(),
            Upload {
                name: "a.png".into(),
                content_type: "image/png".into(),
                data: png,
            },
        )],
    };
    let info = ok(e.handle(up), 200);
    let url = info[0]["url"]
        .as_str()
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_owned();
    let id = info[0]["id"].as_str().unwrap().to_owned();
    e.subscribe(Subscription {
        collection: "docs".into(),
        owner: true,
        ..Default::default()
    })
    .unwrap();
    ok(
        e.handle(
            Request::post(
                "/api/moul/docs/records",
                json!({"file": {"id": id, "url": url}}),
            )
            .owner(),
        ),
        201,
    );
    let d = e.take_deliveries();
    let live = d[0].event.record["file"]["url"].as_str().unwrap();
    assert!(live.contains("?sig="), "{live}");
    assert_eq!(e.handle(Request::from_target("GET", live)).status, 200);
}

#[test]
fn followers_tell_subscribers_about_deletes() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "notes", "fields": [{"name": "text", "type": "text"}],
               "rules": {"viewRule": "text != 'secret'"}}),
    );
    let note = ok(
        e.handle(Request::post("/api/moul/notes/records", json!({"text": "hi"})).owner()),
        201,
    );
    let id = note["id"].as_str().unwrap().to_owned();
    ok(
        e.handle(
            Request::patch(
                &format!("/api/moul/notes/records/{id}"),
                json!({"text": "secret"}),
            )
            .owner(),
        ),
        200,
    );
    let other = ok(
        e.handle(Request::post("/api/moul/notes/records", json!({"text": "bye"})).owner()),
        201,
    );
    e.handle(
        Request::delete(&format!(
            "/api/moul/notes/records/{}",
            other["id"].as_str().unwrap()
        ))
        .owner(),
    );
    let mut f = Engine::open_follower(
        e.into_store(),
        SeededHost::new(3, NOW),
        MemoryBlobs::new(),
        Config::default(),
    )
    .unwrap();
    let other_id = other["id"].as_str().unwrap().to_owned();
    // One subscriber was shown both notes (the host seeds what its first
    // list showed); another was shown nothing.
    let shown = f
        .subscribe(Subscription {
            collection: "notes".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(f.note_visible(shown, &[id.clone(), other_id.clone()]));
    let stranger = f
        .subscribe(Subscription {
            collection: "notes".into(),
            ..Default::default()
        })
        .unwrap();
    f.set_seen(1);
    f.catch_up();
    let deliveries = f.take_deliveries();
    let got: Vec<(Action, String)> = deliveries
        .iter()
        .filter(|d| d.subscription == shown)
        .map(|d| {
            (
                d.event.action,
                d.event.record["id"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    // The note that became hidden and the deleted one both arrive as
    // deletes by id, to the subscriber that had them.
    assert!(got.contains(&(Action::Delete, id)), "{got:?}");
    assert!(got.contains(&(Action::Delete, other_id)), "{got:?}");
    // A subscriber never shown them learns no ids.
    assert!(
        deliveries
            .iter()
            .filter(|d| d.subscription == stranger)
            .all(|d| d.event.action != Action::Delete),
        "{deliveries:?}"
    );
}

#[test]
fn cursors_and_sorts_never_skip_or_break() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "tasks", "fields": [{"name": "order", "type": "number"}, {"name": "group", "type": "text"}]}),
    );
    for i in 0..9 {
        ok(
            e.handle(
                Request::post(
                    "/api/moul/tasks/records",
                    json!({"id": format!("t{}", 8 - i), "order": i % 2, "group": "g"}),
                )
                .owner(),
            ),
            201,
        );
    }
    // Keyword-named fields sort.
    ok(
        e.handle(
            Request::get("/api/moul/tasks/records")
                .query("sort", "-order,group")
                .owner(),
        ),
        200,
    );
    // Paging with a cursor over tied values returns every record once.
    let mut seen = Vec::new();
    let mut after = String::new();
    loop {
        let mut r = Request::get("/api/moul/tasks/records")
            .query("sort", "order")
            .query("perPage", "2")
            .owner();
        if !after.is_empty() {
            r = r.query("after", &after);
        }
        let page = ok(e.handle(r), 200);
        let items = page["items"].as_array().unwrap().clone();
        if items.is_empty() {
            break;
        }
        for it in &items {
            seen.push(it["id"].as_str().unwrap().to_owned());
        }
        after = items.last().unwrap()["id"].as_str().unwrap().to_owned();
    }
    seen.sort();
    assert_eq!(seen, (0..9).map(|i| format!("t{i}")).collect::<Vec<_>>());
    // A huge page number is not a crash: its offset wraps as in Go, and
    // SQLite reads the negative offset as zero.
    let far = ok(
        e.handle(
            Request::get("/api/moul/tasks/records")
                .query("page", &i64::MAX.to_string())
                .owner(),
        ),
        200,
    );
    assert_eq!(far["items"].as_array().unwrap().len(), 9);
}

fn walk(e: &mut Engine, collection: &str, sort: &str, per_page: usize) -> Vec<String> {
    let mut seen = Vec::new();
    let mut after = String::new();
    for _ in 0..100 {
        let mut r = Request::get(&format!("/api/moul/{collection}/records"))
            .query("sort", sort)
            .query("perPage", &per_page.to_string())
            .owner();
        if !after.is_empty() {
            r = r.query("after", &after);
        }
        let items = ok(e.handle(r), 200)["items"].as_array().unwrap().clone();
        if items.is_empty() {
            return seen;
        }
        for it in &items {
            seen.push(it["id"].as_str().unwrap().to_owned());
        }
        after = items.last().unwrap()["id"].as_str().unwrap().to_owned();
    }
    panic!("the walk did not end: {seen:?}");
}

#[test]
fn cursors_handle_null_sort_values() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "items", "fields": [{"name": "n", "type": "number"}, {"name": "note", "type": "text"}]}),
    );
    for (id, n) in [
        ("t1", Some(1)),
        ("t2", Some(2)),
        ("t5", None),
        ("t9", None),
        ("t3", Some(1)),
    ] {
        let mut body = json!({"id": id});
        if let Some(n) = n {
            body["n"] = json!(n);
        }
        ok(
            e.handle(Request::post("/api/moul/items/records", body).owner()),
            201,
        );
    }
    for sort in ["n", "-n", "-createdAt,note", "note,-n", "-note,n"] {
        for per_page in [1, 2, 3] {
            let mut by_cursor = walk(&mut e, "items", sort, per_page);
            let offset: Vec<String> = ok(
                e.handle(
                    Request::get("/api/moul/items/records")
                        .query("sort", sort)
                        .query("perPage", "50")
                        .owner(),
                ),
                200,
            )["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(by_cursor, offset, "sort={sort} perPage={per_page}");
            by_cursor.sort();
            assert_eq!(by_cursor.len(), 5);
        }
    }
}

#[test]
fn a_cascade_may_remove_what_a_restrict_points_from() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "posts", "fields": [{"name": "title", "type": "text"}]}),
    );
    let rel = |target: &str, on_delete: &str| json!({"type": "relation", "relationConfig": {"targetMoul": target, "cardinality": "1:N", "onDelete": on_delete}});
    let mut threads = rel("posts", "CASCADE");
    threads["name"] = json!("post");
    define(&mut e, json!({"name": "threads", "fields": [threads]}));
    let mut images = rel("posts", "CASCADE");
    images["name"] = json!("post");
    define(&mut e, json!({"name": "images", "fields": [images]}));
    let mut thread = rel("threads", "CASCADE");
    thread["name"] = json!("thread");
    let mut image = rel("images", "RESTRICT");
    image["name"] = json!("image");
    define(
        &mut e,
        json!({"name": "comments", "fields": [thread, image]}),
    );
    let post = ok(
        e.handle(Request::post("/api/moul/posts/records", json!({"title": "p"})).owner()),
        201,
    );
    let pid = post["id"].as_str().unwrap();
    let t = ok(
        e.handle(Request::post("/api/moul/threads/records", json!({"post": pid})).owner()),
        201,
    );
    let i = ok(
        e.handle(Request::post("/api/moul/images/records", json!({"post": pid})).owner()),
        201,
    );
    ok(
        e.handle(
            Request::post(
                "/api/moul/comments/records",
                json!({"thread": t["id"], "image": i["id"]}),
            )
            .owner(),
        ),
        201,
    );
    // The comment that RESTRICTs the image goes with the thread: allowed.
    let r = e.handle(Request::delete(&format!("/api/moul/posts/records/{pid}")).owner());
    assert_eq!(r.status, 204, "{:?}", r.body);
    for c in ["threads", "images", "comments"] {
        assert_eq!(
            ok(
                e.handle(Request::get(&format!("/api/moul/{c}/records")).owner()),
                200
            )["totalItems"],
            0,
            "{c}"
        );
    }
    // A comment outside the cascade still blocks it.
    let post = ok(
        e.handle(Request::post("/api/moul/posts/records", json!({"title": "q"})).owner()),
        201,
    );
    let pid = post["id"].as_str().unwrap();
    let i = ok(
        e.handle(Request::post("/api/moul/images/records", json!({"post": pid})).owner()),
        201,
    );
    ok(
        e.handle(Request::post("/api/moul/comments/records", json!({"image": i["id"]})).owner()),
        201,
    );
    let r = e.handle(Request::delete(&format!("/api/moul/posts/records/{pid}")).owner());
    assert_eq!(r.status, 400);
    assert!(r.message().contains("RESTRICT"), "{}", r.message());
}

#[test]
fn exports_sort_by_keyword_named_fields() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "items", "fields": [{"name": "order", "type": "number"}]}),
    );
    ok(
        e.handle(Request::post("/api/moul/items/records", json!({"order": 2})).owner()),
        201,
    );
    let r = e.handle(
        Request::get("/api/moul/items/export")
            .query("sort", "-order")
            .owner(),
    );
    assert_eq!(r.status, 200, "{:?}", r.body);
}

#[test]
fn renames_carry_relations_and_file_references() {
    let mut e = engine();
    let c = define(
        &mut e,
        json!({"name": "people", "fields": [
            {"name": "boss", "type": "relation", "relationConfig": {"targetMoul": "people", "cardinality": "1:N"}},
            {"name": "photo", "type": "file"},
        ]}),
    );
    let mut fields = c["fields"].clone();
    fields[1]["name"] = json!("avatar");
    let renamed = ok(
        e.handle(
            Request::patch(
                "/api/moul/people",
                json!({"name": "staff", "fields": fields}),
            )
            .owner(),
        ),
        200,
    );
    assert_eq!(
        renamed["fields"][0]["relationConfig"]["targetMoul"],
        "staff"
    );
    // A null rule leaves the stored one, as in Go.
    ok(
        e.handle(
            Request::patch("/api/moul/staff", json!({"rules": {"viewRule": "1 = 1"}})).owner(),
        ),
        200,
    );
    let kept = ok(
        e.handle(Request::patch("/api/moul/staff", json!({"rules": {"viewRule": null}})).owner()),
        200,
    );
    assert_eq!(kept["rules"]["viewRule"], "1 = 1");
}

#[test]
fn imports_replace_credentials_and_are_logged() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    let (id, _) = sign_up(&mut e, "old");
    let before = e.changes(0, 1000).len();
    let r = ok(
        e.handle(
            Request::post(
                "/api/moul/users/import",
                json!([{"id": id, "username": "new", "email": "new@example.com"}]),
            )
            .query("mode", "replace")
            .owner(),
        ),
        200,
    );
    assert_eq!(r["inserted"], 1);
    let signin = e.handle(Request::post(
        "/api/moul/users/auth-with-password",
        json!({"identity": "new", "password": "Password123!"}),
    ));
    assert_eq!(
        signin.status, 400,
        "the old password must not open the new record"
    );
    // A refused atomic import is logged too, as refused.
    let r = e.handle(Request::post("/api/moul/users/import", json!([{"username": "x"}])).owner());
    assert_eq!(r.status, 400);
    let log = e.changes(0, 1000);
    assert_eq!(log.len(), before + 2);
    assert_eq!(log.last().unwrap().status, 400);
    // An unparsable JSON body is refused.
    let bad = Request::from_http(
        "POST",
        "/api/moul/users/import",
        "",
        vec![("Content-Type".into(), "application/json".into())],
        b"[{",
    )
    .owner();
    assert_eq!(e.handle(bad).status, 400);
}

#[test]
fn garbage_tokens_are_not_stored_on_logout() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    for i in 0..5 {
        ok(
            e.handle(Request::new("POST", "/api/moul/users/logout").bearer(&format!("junk{i}"))),
            200,
        );
    }
    let n: i64 = e
        .store()
        .conn()
        .query_row("SELECT count(*) FROM writer._revoked_tokens", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn cascades_reach_the_change_log_and_followers() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "teams", "fields": [{"name": "name", "type": "text"}]}),
    );
    define(
        &mut e,
        json!({"name": "members", "fields": [
            {"name": "team", "type": "relation", "relationConfig": {"targetMoul": "teams", "cardinality": "1:N", "onDelete": "CASCADE"}},
        ]}),
    );
    define(
        &mut e,
        json!({"name": "notes", "fields": [
            {"name": "team", "type": "relation", "relationConfig": {"targetMoul": "teams", "cardinality": "1:N"}},
        ]}),
    );
    let team = ok(
        e.handle(Request::post("/api/moul/teams/records", json!({"name": "a"})).owner()),
        201,
    );
    let tid = team["id"].as_str().unwrap().to_owned();
    let member = ok(
        e.handle(Request::post("/api/moul/members/records", json!({"team": tid})).owner()),
        201,
    );
    let note = ok(
        e.handle(Request::post("/api/moul/notes/records", json!({"team": tid})).owner()),
        201,
    );
    let seen = e.changes(0, 1000).last().unwrap().seq;
    e.handle(Request::delete(&format!("/api/moul/teams/records/{tid}")).owner());
    let last = e.changes(seen, 10).pop().unwrap();
    let effects: Vec<(String, String)> = last
        .effects
        .iter()
        .map(|x| (x.record.clone(), x.op.clone()))
        .collect();
    assert!(
        effects.contains(&(member["id"].as_str().unwrap().to_owned(), "delete".into())),
        "{effects:?}"
    );
    assert!(
        effects.contains(&(note["id"].as_str().unwrap().to_owned(), "update".into())),
        "{effects:?}"
    );

    let mut f = Engine::open_follower(
        e.into_store(),
        SeededHost::new(4, NOW),
        MemoryBlobs::new(),
        Config::default(),
    )
    .unwrap();
    f.subscribe(Subscription {
        collection: "members".into(),
        owner: true,
        ..Default::default()
    })
    .unwrap();
    f.subscribe(Subscription {
        collection: "notes".into(),
        owner: true,
        ..Default::default()
    })
    .unwrap();
    f.set_seen(seen);
    f.catch_up();
    let got: Vec<(String, Action)> = f
        .take_deliveries()
        .into_iter()
        .map(|d| (d.event.moul, d.event.action))
        .collect();
    assert!(got.contains(&("members".into(), Action::Delete)), "{got:?}");
    assert!(got.contains(&("notes".into(), Action::Update)), "{got:?}");
}

#[test]
fn a_deeply_nested_filter_is_refused_not_a_crash() {
    let mut e = engine();
    posts(&mut e);
    let filter = format!("{}title = 'x'{}", "(".repeat(5000), ")".repeat(5000));
    let r = e.handle(Request::get("/api/moul/posts/records").query("filter", &filter));
    assert_eq!(r.status, 400, "{:?}", r.body);
}

#[test]
fn duplicate_values_answer_like_go() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "users", "type": "auth", "rules": {"createRule": ""}}),
    );
    sign_up(&mut e, "bob");
    let dup = e.handle(Request::post(
        "/api/moul/users/records",
        json!({"username": "bob", "email": "other@example.com"}),
    ));
    assert_eq!(
        (dup.status, dup.message()),
        (400, "Username or Email already exists")
    );
    define(
        &mut e,
        json!({"name": "pages", "fields": [{"name": "slug", "type": "text", "unique": true}]}),
    );
    ok(
        e.handle(Request::post("/api/moul/pages/records", json!({"slug": "a"})).owner()),
        201,
    );
    let b = ok(
        e.handle(Request::post("/api/moul/pages/records", json!({"slug": "b"})).owner()),
        201,
    );
    let dup = e.handle(Request::post("/api/moul/pages/records", json!({"slug": "a"})).owner());
    assert_eq!(
        (dup.status, dup.message()),
        (400, "Value for field \"slug\" already exists")
    );
    let id = b["id"].as_str().unwrap();
    let dup = e.handle(
        Request::patch(
            &format!("/api/moul/pages/records/{id}"),
            json!({"slug": "a"}),
        )
        .owner(),
    );
    assert_eq!(
        (dup.status, dup.message()),
        (400, "Value for field \"slug\" already exists")
    );
}

fn upload_png(e: &mut Engine, token: Option<&str>) -> Value {
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01rest".to_vec();
    let mut up = Request::new("POST", "/api/upload");
    up = match token {
        Some(t) => up.bearer(t),
        None => up.owner(),
    };
    up.body = Body::Form {
        fields: vec![],
        files: vec![(
            "file".into(),
            Upload {
                name: "a.png".into(),
                content_type: "image/png".into(),
                data: png,
            },
        )],
    };
    let info = ok(e.handle(up), 200);
    let url = info[0]["url"]
        .as_str()
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_owned();
    json!({"id": info[0]["id"], "url": url})
}

#[test]
fn swapping_file_field_names_keeps_their_files() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    let (_, owner_token) = sign_up(&mut e, "ann");
    let (_, editor_token) = sign_up(&mut e, "ben");
    let c = define(
        &mut e,
        json!({"name": "docs", "fields": [{"name": "a", "type": "file"}, {"name": "b", "type": "file"}],
               "rules": {"createRule": "", "updateRule": "", "viewRule": ""}}),
    );
    let fa = upload_png(&mut e, Some(&owner_token));
    let fb = upload_png(&mut e, Some(&owner_token));
    let doc = ok(
        e.handle(
            Request::post("/api/moul/docs/records", json!({"a": fa, "b": fb})).bearer(&owner_token),
        ),
        201,
    );
    // Swap the names: the file that was in a is now in b, and so on.
    let mut fields = c["fields"].clone();
    fields[0]["name"] = json!("b");
    fields[1]["name"] = json!("a");
    ok(
        e.handle(Request::patch("/api/moul/docs", json!({"fields": fields})).owner()),
        200,
    );
    // An editor who uploaded nothing may keep the record's own files.
    let id = doc["id"].as_str().unwrap();
    let r = e.handle(
        Request::patch(
            &format!("/api/moul/docs/records/{id}"),
            json!({"b": fa, "a": fb}),
        )
        .bearer(&editor_token),
    );
    assert_eq!(r.status, 200, "{:?}", r.body);
    // The same file in both fields survives a swap too.
    let both = ok(
        e.handle(
            Request::post("/api/moul/docs/records", json!({"a": fa, "b": fa})).bearer(&owner_token),
        ),
        201,
    );
    let now = ok(e.handle(Request::get("/api/moul/docs").owner()), 200);
    let mut fields = now["fields"].clone();
    fields[0]["name"] = json!("a");
    fields[1]["name"] = json!("b");
    ok(
        e.handle(Request::patch("/api/moul/docs", json!({"fields": fields})).owner()),
        200,
    );
    let _ = both;
}

#[test]
fn schema_file_renames_map_targets_at_once() {
    let mut e = engine();
    let rel = |name: &str, target: &str| json!({"name": name, "type": "relation", "relationConfig": {"targetMoul": target, "cardinality": "1:N"}});
    ok(
        e.handle(
            Request::post(
                "/api/schema",
                json!({"collections": [
                    {"id": "aaaaaaaaaaaaaaa", "name": "a"},
                    {"id": "bbbbbbbbbbbbbbb", "name": "b"},
                    {"id": "xxxxxxxxxxxxxxx", "name": "x", "fields": [rel("ra", "a"), rel("rb", "b")]},
                    {"id": "ccccccccccccccc", "name": "comments", "fields": [rel("parent", "comments")]},
                ]}),
            )
            .owner(),
        ),
        200,
    );
    let targets = |e: &mut Engine, name: &str| -> Vec<String> {
        ok(
            e.handle(Request::get(&format!("/api/moul/{name}")).owner()),
            200,
        )["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                f["relationConfig"]["targetMoul"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned()
            })
            .collect()
    };
    // A swap with the targets written in new names; comments renamed to
    // archive and a new comments made with a self relation. The new one
    // has its own id: without one it would match the stored comments by
    // name, which the rename claims by id, and the file is refused.
    let file = json!({"collections": [
        {"id": "aaaaaaaaaaaaaaa", "name": "c"},
        {"id": "bbbbbbbbbbbbbbb", "name": "a"},
        {"id": "xxxxxxxxxxxxxxx", "name": "x", "fields": [rel("ra", "c"), rel("rb", "a")]},
        {"id": "ccccccccccccccc", "name": "archive", "fields": [rel("parent", "archive")]},
        {"id": "ddddddddddddddd", "name": "comments", "fields": [rel("parent", "comments")]},
    ]});
    ok(
        e.handle(Request::post("/api/schema", file.clone()).owner()),
        200,
    );
    assert_eq!(targets(&mut e, "x"), ["c", "a"]);
    assert_eq!(targets(&mut e, "comments"), ["comments"]);
    assert_eq!(targets(&mut e, "archive"), ["archive"]);
    // Applying the same file again changes nothing.
    let again = ok(e.handle(Request::post("/api/schema", file).owner()), 200);
    assert_eq!(again["updated"], json!([]));
    assert_eq!(targets(&mut e, "x"), ["c", "a"]);
    // A stale target that names no collection follows the rename.
    ok(
        e.handle(
            Request::post(
                "/api/schema",
                json!({"collections": [
                    {"id": "aaaaaaaaaaaaaaa", "name": "d"},
                    {"id": "xxxxxxxxxxxxxxx", "name": "x", "fields": [rel("ra", "c"), rel("rb", "a")]},
                ]}),
            )
            .owner(),
        ),
        200,
    );
    assert_eq!(targets(&mut e, "x"), ["d", "a"]);
    // A null rule in the file keeps the stored rule.
    ok(
        e.handle(Request::patch("/api/moul/d", json!({"rules": {"deleteRule": "1 = 2"}})).owner()),
        200,
    );
    ok(
        e.handle(
            Request::post(
                "/api/schema",
                json!({"collections": [{"id": "aaaaaaaaaaaaaaa", "name": "d", "rules": {"deleteRule": null}}]}),
            )
            .owner(),
        ),
        200,
    );
    assert_eq!(
        ok(e.handle(Request::get("/api/moul/d").owner()), 200)["rules"]["deleteRule"],
        "1 = 2"
    );
}

#[test]
fn schema_file_refuses_missing_targets_and_doubles() {
    let mut e = engine();
    let rel = |name: &str, target: &str| json!({"name": name, "type": "relation", "relationConfig": {"targetMoul": target, "cardinality": "1:N"}});
    let apply = |e: &mut Engine, file: Value, prune: bool| {
        let mut req = Request::post("/api/schema", file).owner();
        if prune {
            req = req.query("prune", "true");
        }
        e.handle(req)
    };
    let refused = |r: Response, words: &[&str]| {
        assert_eq!(r.status, 400, "{:?}", r.body);
        for w in words {
            assert!(r.message().contains(w), "{:?} lacks {w}", r.message());
        }
    };

    // A relation to a collection that does not exist, and nothing is made.
    let tasks = json!({"name": "tasks", "fields": [rel("team", "teams")]});
    refused(
        apply(&mut e, json!({"collections": [tasks.clone()]}), false),
        &["\"team\"", "\"tasks\"", "\"teams\""],
    );
    assert_eq!(
        e.handle(Request::get("/api/moul/tasks").owner()).status,
        404
    );
    // One the engine holds, or the file lists, is fine; with `prune`, only
    // what the file lists stays.
    let teams = define(
        &mut e,
        json!({"name": "teams", "fields": [{"name": "name", "type": "text"}]}),
    );
    ok(
        apply(&mut e, json!({"collections": [tasks.clone()]}), false),
        200,
    );
    refused(
        apply(&mut e, json!({"collections": [tasks.clone()]}), true),
        &["\"teams\""],
    );
    assert_eq!(
        e.handle(Request::get("/api/moul/teams").owner()).status,
        200
    );

    // Two entries with one name, in any case, or one id.
    refused(
        apply(
            &mut e,
            json!({"collections": [{"name": "notes"}, {"name": "Notes"}]}),
            false,
        ),
        &["\"Notes\"", "twice"],
    );
    refused(
        apply(
            &mut e,
            json!({"collections": [
                {"id": "nnnnnnnnnnnnnnn", "name": "one"},
                {"id": "nnnnnnnnnnnnnnn", "name": "two"},
            ]}),
            false,
        ),
        &["\"two\"", "\"one\"", "nnnnnnnnnnnnnnn"],
    );
    assert_eq!(e.handle(Request::get("/api/moul/one").owner()).status, 404);
    // One entry matching by name the collection another renames by id.
    refused(
        apply(
            &mut e,
            json!({"collections": [
                {"name": "teams", "fields": [{"name": "label", "type": "text"}]},
                {"id": teams["id"], "name": "squads"},
            ]}),
            false,
        ),
        &["\"teams\""],
    );
    let kept = ok(e.handle(Request::get("/api/moul/teams").owner()), 200);
    assert_eq!(kept["fields"][0]["name"], "name", "nothing was applied");
    // The same two entries the other way round: the rename comes first.
    refused(
        apply(
            &mut e,
            json!({"collections": [
                {"id": teams["id"], "name": "squads"},
                {"name": "teams", "fields": [{"name": "label", "type": "text"}]},
            ]}),
            false,
        ),
        &["\"teams\"", "give each its id"],
    );
    let kept = ok(e.handle(Request::get("/api/moul/teams").owner()), 200);
    assert_eq!(kept["fields"][0]["name"], "name", "nothing was applied");
    assert_eq!(
        e.handle(Request::get("/api/moul/squads").owner()).status,
        404
    );
    // Names match in any case, so the order of the entries does not matter
    // when only the case differs either.
    for file in [
        json!({"collections": [
            {"id": teams["id"], "name": "squads"},
            {"name": "Teams", "fields": [{"name": "label", "type": "text"}]},
        ]}),
        json!({"collections": [
            {"name": "Teams", "fields": [{"name": "label", "type": "text"}]},
            {"id": teams["id"], "name": "squads"},
        ]}),
    ] {
        refused(
            apply(&mut e, file, false),
            &["\"teams\"", "give each its id"],
        );
        let kept = ok(e.handle(Request::get("/api/moul/teams").owner()), 200);
        assert_eq!(kept["name"], "teams", "nothing was applied");
        assert_eq!(kept["fields"][0]["name"], "name", "nothing was applied");
    }
    // An entry without an id that names a collection in another case
    // describes it: it changes that collection, renamed to the new case.
    let r = ok(
        apply(
            &mut e,
            json!({"collections": [{"name": "TEAMS", "fields": [{"name": "name", "type": "text"}]}]}),
            false,
        ),
        200,
    );
    assert_eq!(r["updated"], json!(["TEAMS"]), "{r}");
    let renamed = ok(e.handle(Request::get("/api/moul/TEAMS").owner()), 200);
    assert_eq!(renamed["id"], teams["id"]);
}

#[test]
fn a_replace_import_keeps_unchanged_logins_and_logs_refusals() {
    let mut e = engine();
    define(&mut e, json!({"name": "users", "type": "auth"}));
    sign_up(&mut e, "alice");
    sign_up(&mut e, "carol");
    // Export, then restore the same file with replace: logins survive.
    let out = e.handle(Request::get("/api/moul/users/export").owner());
    let moul_core::ResponseBody::Bytes { data, .. } = &out.body else {
        panic!()
    };
    let mut restore = Request::new("POST", "/api/moul/users/import")
        .query("mode", "replace")
        .owner();
    restore.body = Body::Bytes {
        content_type: "application/json".into(),
        data: data.clone(),
    };
    assert_eq!(ok(e.handle(restore), 200)["inserted"], 2);
    for name in ["alice", "carol"] {
        ok(
            e.handle(Request::post(
                "/api/moul/users/auth-with-password",
                json!({"identity": name, "password": "Password123!"}),
            )),
            200,
        );
    }
    // A refused import is logged with its reason, and replays with it.
    let bad = Request::post("/api/moul/users/import", json!([{"username": "x"}]))
        .owner()
        .with_id("studio:9");
    let first = e.handle(bad.clone());
    assert_eq!(first.status, 400);
    let log = e.changes(0, 1000);
    assert!(
        log.last()
            .unwrap()
            .error
            .as_deref()
            .unwrap_or("")
            .contains("row 1"),
        "{:?}",
        log.last()
    );
    assert!(e.handle(bad).message().contains("row 1"));
    // A blank JSON body imports nothing, as in Go.
    let blank = Request::from_http(
        "POST",
        "/api/moul/users/import",
        "",
        vec![("Content-Type".into(), "application/json".into())],
        b"  \n",
    )
    .owner();
    assert_eq!(ok(e.handle(blank), 200)["total"], 0);
}

#[test]
fn webhook_secrets_go_with_their_collection() {
    let mut e = engine();
    define(
        &mut e,
        json!({"id": "posts_id_000001", "name": "posts",
               "webhooks": [{"id": "w1", "url": "https://a/x", "Secret": "OLD"}]}),
    );
    let got = ok(e.handle(Request::get("/api/moul/posts").owner()), 200);
    assert!(!got.to_string().contains("OLD"));
    let count = |e: &Engine| -> i64 {
        e.store()
            .conn()
            .query_row("SELECT count(*) FROM writer._webhook_secrets", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert_eq!(count(&e), 1);
    e.handle(Request::delete("/api/moul/posts").owner());
    assert_eq!(count(&e), 0);
}

#[test]
fn reads_name_the_collections_a_live_query_depends_on() {
    let mut e = engine();
    let users = define(&mut e, json!({"name": "users", "type": "auth"}));
    let teams = define(
        &mut e,
        json!({"name": "teams", "fields": [{"name": "name", "type": "text"}]}),
    );
    let tasks = define(
        &mut e,
        json!({
            "name": "tasks",
            "fields": [
                {"name": "title", "type": "text"},
                {"name": "team", "type": "relation", "relationConfig": {"targetMoul": "teams", "cardinality": "1:N"}},
            ],
            "rules": {"listRule": "", "viewRule": ""},
        }),
    );
    let notes = define(
        &mut e,
        json!({
            "name": "notes",
            "fields": [{"name": "body", "type": "text"}],
            "rules": {"viewRule": "exists('teams', 'id != \"\"')"},
        }),
    );
    let id = |v: &Value| v["id"].as_str().unwrap().to_owned();
    let reads = |r: &Response| r.header_value("Moul-Reads").map(str::to_owned);

    // The collection itself.
    let r = e.handle(Request::get("/api/moul/tasks/records"));
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(reads(&r), Some(id(&tasks)));

    // A filter through a relation, and an expand, read the target too.
    let r = e.handle(Request::get("/api/moul/tasks/records").query("filter", "team.name = 'x'"));
    assert_eq!(r.status, 200, "{:?}", r.body);
    let mut both = [id(&tasks), id(&teams)];
    both.sort();
    assert_eq!(reads(&r), Some(both.join(",")));
    // An expand reads the targets of the rows it expands; with no rows it
    // reads nothing else, and a row added later changes `tasks` anyway.
    let r = e.handle(Request::get("/api/moul/tasks/records").query("expand", "team"));
    assert_eq!(reads(&r), Some(id(&tasks)));
    let team = ok(
        e.handle(Request::post("/api/moul/teams/records", json!({"name": "a"})).owner()),
        201,
    );
    ok(
        e.handle(
            Request::post(
                "/api/moul/tasks/records",
                json!({"title": "t", "team": id(&team)}),
            )
            .owner(),
        ),
        201,
    );
    let r = e.handle(Request::get("/api/moul/tasks/records").query("expand", "team"));
    assert_eq!(reads(&r), Some(both.join(",")));

    // A rule that can read any table makes the read depend on everything.
    let r = e.handle(Request::get("/api/moul/notes/records").owner());
    assert_eq!(reads(&r).as_deref(), Some("*"), "{:?}", id(&notes));

    // A rule's `@collection.<name>` conditions read that collection's table
    // directly; the read depends on it too.
    let members = define(
        &mut e,
        json!({"name": "members", "fields": [{"name": "user", "type": "text"}]}),
    );
    let projects = define(
        &mut e,
        json!({
            "name": "projects",
            "fields": [{"name": "title", "type": "text"}],
            "rules": {"listRule": "@collection.members.user = @request.auth.id"},
        }),
    );
    let r = e.handle(Request::get("/api/moul/projects/records"));
    assert_eq!(r.status, 200, "{:?}", r.body);
    let mut through = [id(&projects), id(&members)];
    through.sort();
    assert_eq!(reads(&r), Some(through.join(",")));

    // A signed-in caller's own record feeds `@request.auth`.
    ok(
        e.handle(Request::post(
            "/api/moul/users/records",
            json!({"username": "ana", "email": "ana@example.com", "password": "Password123!", "passwordConfirm": "Password123!"}),
        )),
        201,
    );
    let token = ok(
        e.handle(Request::post(
            "/api/moul/users/auth-with-password",
            json!({"identity": "ana@example.com", "password": "Password123!"}),
        )),
        200,
    )["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = e.handle(Request::get("/api/moul/tasks/records").bearer(&token));
    let mut mine = [id(&tasks), id(&users)];
    mine.sort();
    assert_eq!(reads(&r), Some(mine.join(",")));

    // Writes and failures carry no read set.
    let r = e.handle(Request::post(
        "/api/moul/tasks/records",
        json!({"title": "a"}),
    ));
    assert_eq!(reads(&r), None);
    let r = e.handle(Request::get("/api/moul/nope/records"));
    assert_eq!(reads(&r), None);
}

#[test]
fn importing_a_database_file_migrates_it() {
    let mut a = engine();
    posts(&mut a);
    ok(
        a.handle(Request::post("/api/moul/posts/records", json!({"title": "kept"})).owner()),
        201,
    );
    let file = a.store().export().unwrap();
    let mut b = engine();
    b.import_database(&file).unwrap();
    let r = ok(
        b.handle(Request::get("/api/moul/posts/records").owner()),
        200,
    );
    assert_eq!(r["items"][0]["title"], "kept");
    // The change log continues after the imported one.
    let r = b.handle(Request::post("/api/moul/posts/records", json!({"title": "new"})).owner());
    assert_eq!(r.status, 201);
    let seq: i64 = r.header_value("Moul-Change").unwrap().parse().unwrap();
    assert!(seq > 1, "{seq}");
}

#[test]
fn a_file_that_cannot_be_migrated_is_refused_and_the_data_stays() {
    let mut e = engine();
    posts(&mut e);
    ok(
        e.handle(Request::post("/api/moul/posts/records", json!({"title": "kept"})).owner()),
        201,
    );
    // A shared store from a newer engine passes the import checks but
    // cannot be migrated.
    let mut newer = Store::open_in_memory().unwrap();
    newer
        .write(|tx| tx.execute_batch("CREATE TABLE notes (body TEXT); PRAGMA user_version = 99;"))
        .unwrap();
    let file = newer.export().unwrap();
    let (before, commit) = (
        e.store().export().unwrap(),
        e.store().volume().last_commit(),
    );

    let err = e.import_database(&file).unwrap_err();
    assert!(err.to_string().contains("format 99"), "{err}");
    assert!(
        e.store().export().unwrap() == before,
        "the shared store changed"
    );
    assert_eq!(e.store().volume().last_commit(), commit, "a commit landed");
    let r = ok(
        e.handle(Request::get("/api/moul/posts/records").owner()),
        200,
    );
    assert_eq!(r["items"][0]["title"], "kept");
    let r = e.handle(Request::post("/api/moul/posts/records", json!({"title": "after"})).owner());
    assert_eq!(r.status, 201, "{:?}", r.body);
}

#[test]
fn dot_ids_are_refused_and_the_log_has_a_tail() {
    let mut e = engine();
    posts(&mut e);
    for id in [".", ".."] {
        let r = e.handle(
            Request::post("/api/moul/posts/records", json!({"id": id, "title": "x"})).owner(),
        );
        assert_eq!(r.status, 400, "{id}: {:?}", r.body);
    }
    for i in 0..5 {
        ok(
            e.handle(
                Request::post("/api/moul/posts/records", json!({"title": format!("t{i}")})).owner(),
            ),
            201,
        );
    }
    let all = ok(e.handle(Request::get("/api/changes").owner()), 200);
    let seqs: Vec<i64> = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["seq"].as_i64().unwrap())
        .collect();
    let tail = ok(
        e.handle(Request::get("/api/changes").query("tail", "2").owner()),
        200,
    );
    let got: Vec<i64> = tail["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(got, seqs[seqs.len() - 2..].to_vec());
}

// ---------------------------------------------------------------- rules on save

/// The 400 a refused save answers: its message and the rule it names.
fn refused(r: Response) -> (String, String) {
    let body = ok(r, 400);
    (
        body["message"].as_str().unwrap().to_owned(),
        body["rule"].as_str().unwrap_or_default().to_owned(),
    )
}

#[test]
fn a_rule_that_does_not_compile_is_refused_when_saved() {
    let mut e = engine();
    define(
        &mut e,
        json!({"name": "teams", "fields": [{"name": "owner", "type": "text"}]}),
    );
    define(
        &mut e,
        json!({"name": "members", "fields": [{"name": "user", "type": "text"}]}),
    );
    let tasks = json!({
        "name": "tasks",
        "fields": [
            {"name": "title", "type": "text"},
            {"name": "status", "type": "text"},
            {"name": "owner", "type": "text"},
            {"name": "team", "type": "relation",
             "relationConfig": {"targetMoul": "teams", "cardinality": "1:N"}},
        ],
    });
    let with_rules = |rules: Value| {
        let mut c = tasks.clone();
        c["rules"] = rules;
        c
    };

    // Not made at all.
    let (message, rule) = refused(
        e.handle(
            Request::post(
                "/api/moul",
                with_rules(json!({"listRule": "!(status = 'x')"})),
            )
            .owner(),
        ),
    );
    assert_eq!(
        message,
        "Invalid listRule: failed to tokenize filter: unexpected character '!' at position 0"
    );
    assert_eq!(rule, "listRule");
    assert_eq!(
        e.handle(Request::get("/api/moul/tasks").owner()).status,
        404
    );

    // Every rule that runs can be saved.
    let fine = json!({
        "listRule": "team.owner = @request.auth.id || @collection.members.user = @request.auth.id",
        "viewRule": "exists('members', 'user = ?', @request.auth.id) || get('members', 'user = ?', owner) != nil",
        "createRule": "@request.auth.id != '' && @request.body.owner = @request.auth.id",
        "updateRule": "true",
        "deleteRule": "",
        "subscribeRule": "@collection.members.user = @request.auth.id",
    });
    define(&mut e, with_rules(fine.clone()));
    let list = |e: &mut Engine| e.handle(Request::get("/api/moul/tasks/records")).status;
    assert_eq!(list(&mut e), 200);

    for (rules, want) in [
        (
            json!({"listRule": "(status = 'x') = false"}),
            ("listRule", r#"unexpected token "=" at position 5"#),
        ),
        (
            json!({"listRule": "@request.body.title != ''"}),
            (
                "listRule",
                "a list rule cannot read @request.body.title: a list has no body",
            ),
        ),
        (
            json!({"listRule": "exists('members', 'user = ?', @request.auth.id)"}),
            (
                "listRule",
                "a list rule cannot call exists(): use @collection.<name>.<field> or a relation path",
            ),
        ),
        (
            json!({"listRule": "@collection.membres.user = @request.auth.id"}),
            (
                "listRule",
                r#"unknown collection "membres" in "@collection.membres.user""#,
            ),
        ),
        (
            json!({"viewRule": "ownr = @request.auth.id || titel = ''"}),
            ("viewRule", r#"invalid field "ownr"; invalid field "titel""#),
        ),
        (
            json!({"updateRule": "team.owner = @request.auth.id"}),
            (
                "updateRule",
                r#""team.owner" follows the relation "team", which only a list rule can do"#,
            ),
        ),
    ] {
        let (message, rule) = refused(
            e.handle(Request::patch("/api/moul/tasks", json!({"rules": rules.clone()})).owner()),
        );
        assert_eq!(
            message,
            format!("Invalid {}: {}", want.0, want.1),
            "{rules}"
        );
        assert_eq!(rule, want.0);
    }
    // Nothing was stored, and lists still work.
    assert_eq!(
        ok(e.handle(Request::get("/api/moul/tasks").owner()), 200)["rules"],
        fine
    );
    assert_eq!(list(&mut e), 200);

    // A change that breaks a rule which compiled is refused too.
    let mut fields = tasks["fields"].clone();
    fields
        .as_array_mut()
        .unwrap()
        .retain(|f| f["name"] != "owner");
    let (message, rule) =
        refused(e.handle(Request::patch("/api/moul/tasks", json!({"fields": fields})).owner()));
    assert_eq!(message, r#"Invalid viewRule: invalid field "owner""#);
    assert_eq!(rule, "viewRule");
    let (message, _) = refused(
        e.handle(
            Request::patch(
                "/api/moul/teams",
                json!({"fields": [{"name": "boss", "type": "text"}],
                   "rules": {"listRule": "own = ''"}}),
            )
            .owner(),
        ),
    );
    assert_eq!(message, r#"Invalid listRule: invalid field "own""#);
}

#[test]
fn a_schema_file_with_a_rule_that_does_not_compile_changes_nothing() {
    let mut e = engine();
    // Rules are checked once the whole file is applied: one may reach a
    // collection the file makes after it.
    let projects = |list_rule: &str| {
        json!({
            "name": "projects",
            "fields": [{"name": "team", "type": "relation",
                        "relationConfig": {"targetMoul": "teams", "cardinality": "1:N"}}],
            "rules": {"listRule": list_rule, "viewRule": "@collection.teams.owner = @request.auth.id"},
        })
    };
    let teams = json!({"name": "teams", "fields": [{"name": "owner", "type": "text"}]});
    let file =
        |list_rule: &str| json!({"format": 1, "collections": [projects(list_rule), teams.clone()]});
    let applied = ok(
        e.handle(Request::post("/api/schema", file("team.owner = @request.auth.id")).owner()),
        200,
    );
    assert_eq!(applied["created"], json!(["projects", "teams"]));

    // A broken rule refuses the whole file, naming the collection.
    let mut broken = file("team.ownr = @request.auth.id");
    broken["collections"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "notes"}));
    let (message, rule) = refused(e.handle(Request::post("/api/schema", broken).owner()));
    assert_eq!(
        message,
        r#"Invalid listRule of the collection "projects": invalid field "ownr" in path "team.ownr""#
    );
    assert_eq!(rule, "listRule");
    assert_eq!(
        e.handle(Request::get("/api/moul/notes").owner()).status,
        404
    );
    assert_eq!(
        ok(e.handle(Request::get("/api/moul/projects").owner()), 200)["rules"]["listRule"],
        "team.owner = @request.auth.id"
    );
    assert_eq!(
        e.handle(Request::get("/api/moul/projects/records")).status,
        200
    );
}

#[test]
fn a_go_database_holding_a_broken_rule_opens_and_is_fixed_by_saving() {
    let store = Store::open_in_memory().unwrap();
    moul_fixtures::Fixtures::default()
        .db("cbc2a77b5c6e0079")
        .unwrap()
        .load_into(store.conn())
        .unwrap();
    // The Go engine stored any rule text.
    store
        .conn()
        .execute(
            "UPDATE _moul SET rules = json_set(rules, '$.listRule', '!(name = ''x'')') WHERE name = 'categories'",
            [],
        )
        .unwrap();
    let mut e = Engine::open(
        store,
        SeededHost::new(9, NOW),
        MemoryBlobs::new(),
        Config::default(),
    )
    .unwrap();
    let c = ok(e.handle(Request::get("/api/moul/categories").owner()), 200);
    assert_eq!(c["rules"]["listRule"], "!(name = 'x')");
    // The owner lists without the rule; anyone else meets it.
    ok(
        e.handle(Request::get("/api/moul/categories/records").owner()),
        200,
    );
    assert_eq!(
        e.handle(Request::get("/api/moul/categories/records"))
            .status,
        500
    );

    // Saving the collection again means fixing the rule.
    let (message, rule) = refused(
        e.handle(Request::patch("/api/moul/categories", json!({"name": "topics"})).owner()),
    );
    assert!(message.starts_with("Invalid listRule: "), "{message}");
    assert_eq!(rule, "listRule");
    ok(
        e.handle(
            Request::patch(
                "/api/moul/categories",
                json!({"name": "topics", "rules": {"listRule": "name != 'x'"}}),
            )
            .owner(),
        ),
        200,
    );
    assert_eq!(
        ok(e.handle(Request::get("/api/moul/topics/records")), 200)["totalItems"],
        4
    );
}
