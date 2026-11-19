//! Mail, webhook calls and jobs left as rows for a native server, the
//! settings it reads, and signing in through an OAuth provider.

#![cfg(not(target_arch = "wasm32"))]

use std::cell::Cell;
use std::rc::Rc;

use moul_core::{Config, Engine, ExternalProfile, Host, MemoryBlobs, Request, SeededHost};
use moul_store::Store;
use serde_json::{Value, json};

const NOW: i64 = 1_794_961_964_000;

thread_local! {
    static CLOCK: Rc<Cell<i64>> = Rc::new(Cell::new(NOW));
}

/// Seeded randomness and a clock the test moves.
struct TestHost(SeededHost, Rc<Cell<i64>>);

impl Host for TestHost {
    fn now_ms(&mut self) -> i64 {
        self.1.get()
    }
    fn fill_random(&mut self, buf: &mut [u8]) {
        self.0.fill_random(buf)
    }
}

fn advance(_: &mut Engine, ms: i64) {
    CLOCK.with(|c| c.set(c.get() + ms));
}

fn engine() -> Engine {
    CLOCK.with(|c| c.set(NOW));
    let config = Config {
        admin_key: Some("admin".into()),
        public_url: "https://app.example".into(),
        ..Config::default()
    };
    Engine::open(
        Store::open_in_memory().unwrap(),
        TestHost(SeededHost::new(3, NOW), CLOCK.with(Rc::clone)),
        MemoryBlobs::new(),
        config,
    )
    .unwrap()
}

fn ok(e: &mut Engine, req: Request) -> Value {
    let r = e.handle(req.header("X-Admin-Key", "admin"));
    assert!(r.status < 300, "{} {:?}", r.status, r.json_body());
    r.json_body().clone()
}

#[test]
fn writes_queue_webhook_calls_once_and_retry_them() {
    let mut e = engine();
    ok(
        &mut e,
        Request::post(
            "/api/moul",
            json!({"name": "tasks", "type": "base", "fields": [{"name": "title", "type": "text"}]}),
        ),
    );
    let hook = ok(
        &mut e,
        Request::post(
            "/api/moul/tasks/webhooks",
            json!({"url": "https://hooks.example/t", "events": ["create"], "secret": "s3"}),
        ),
    );
    assert_eq!(hook["secret"], "********", "secrets are not read back");
    assert_eq!(hook["enabled"], true);
    let before = e.handle(
        Request::post(
            "/api/moul/tasks/webhooks",
            json!({"url": "https://x.example", "events": ["create:before"]}),
        )
        .header("X-Admin-Key", "admin"),
    );
    assert_eq!(before.status, 400, "{:?}", before.json_body());

    ok(
        &mut e,
        Request::post("/api/moul/tasks/records", json!({"title": "a"})),
    );
    // An update is not an event this webhook wants.
    let calls = e.due_webhooks(10);
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.url, "https://hooks.example/t");
    assert_eq!(call.event, "create:after");
    assert_eq!(call.secret.as_deref(), Some("s3"));
    assert_eq!(call.collection, "tasks");
    let payload: Value = serde_json::from_str(&call.payload).unwrap();
    assert_eq!(payload["record"]["title"], "a");
    assert_eq!(payload["moul"], "tasks");
    assert!(e.due_webhooks(10).is_empty(), "a taken call is leased");

    e.webhook_done(&call.id, Err("HTTP 500".into()));
    assert!(e.due_webhooks(10).is_empty(), "a retry waits");
    // Thirty seconds later it is due again.
    advance(&mut e, 31_000);
    let again = e.due_webhooks(10);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].attempt, 2);
    e.webhook_done(&again[0].id, Ok(()));
    advance(&mut e, 3_600_000);
    assert!(e.due_webhooks(10).is_empty());

    // A call queued for a webhook deleted since is dropped, not sent.
    ok(
        &mut e,
        Request::post("/api/moul/tasks/records", json!({"title": "b"})),
    );
    let id = hook["id"].as_str().unwrap();
    ok(
        &mut e,
        Request::delete(&format!("/api/moul/tasks/webhooks/{id}")),
    );
    assert!(e.due_webhooks(10).is_empty());
}

#[test]
fn mail_is_sent_once_and_old_mail_expires() {
    let mut e = engine();
    ok(
        &mut e,
        Request::post(
            "/api/moul",
            json!({"name": "users", "type": "auth", "fields": []}),
        ),
    );
    let r = e.handle(Request::post(
        "/api/moul/users/otp/request",
        json!({"email": "a@b.co"}),
    ));
    assert_eq!(r.status, 200);
    let mail = e.due_mail(10);
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].to, "a@b.co");
    assert_eq!(mail[0].kind, "otp");
    assert!(e.due_mail(10).is_empty());
    e.mail_done(&mail[0].id, Ok(()));

    // Mail nobody sent within half an hour is not sent at all.
    e.handle(Request::post(
        "/api/moul/users/otp/request",
        json!({"email": "c@d.co"}),
    ));
    advance(&mut e, 31 * 60 * 1000);
    assert!(e.due_mail(10).is_empty());

    let t = ok(
        &mut e,
        Request::post(
            "/api/moul/users/email-templates/test",
            json!({"email": "me@x.co", "template": "otp"}),
        ),
    );
    assert_eq!(t["message"], "Test email sent/queued successfully");
    let m = e.due_mail(10);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].kind, "test");
    assert!(m[0].body.contains("123456"));
}

#[test]
fn jobs_are_claimed_retried_and_discarded() {
    let mut e = engine();
    let job = ok(
        &mut e,
        Request::post(
            "/api/workers",
            json!({"worker": "SendEmail", "args": {"to": "a@b.co"}, "max_attempts": 2}),
        ),
    );
    assert_eq!(job["state"], "available");
    assert_eq!(job["args"]["to"], "a@b.co");
    ok(
        &mut e,
        Request::post("/api/workers", json!({"worker": "Mine"})),
    );
    let claimed = e.claim_jobs(&["SendEmail"], 10, "node-1");
    assert_eq!(claimed.len(), 1, "only workers the runner knows");
    assert_eq!(claimed[0].attempt, 1);
    e.job_done(&claimed[0].id, Err("no mail".into()));
    let read = ok(
        &mut e,
        Request::get(&format!("/api/workers/{}", claimed[0].id)),
    );
    assert_eq!(read["state"], "available");
    assert_eq!(read["errors"].as_array().unwrap().len(), 1);
    advance(&mut e, 120_000);
    let again = e.claim_jobs(&["SendEmail"], 10, "node-1");
    assert_eq!(again.len(), 1);
    e.job_done(&again[0].id, Err("still no mail".into()));
    let read = ok(
        &mut e,
        Request::get(&format!("/api/workers/{}", again[0].id)),
    );
    assert_eq!(read["state"], "discarded");

    // A job cancelled while it ran stays cancelled when the run fails.
    ok(
        &mut e,
        Request::post("/api/workers", json!({"worker": "SendEmail", "id": "j1"})),
    );
    let running = e.claim_jobs(&["SendEmail"], 10, "node-1");
    assert_eq!(running.len(), 1);
    ok(
        &mut e,
        Request::patch("/api/workers/j1", json!({"state": "cancelled"})),
    );
    e.job_done("j1", Err("network".into()));
    assert_eq!(
        ok(&mut e, Request::get("/api/workers/j1"))["state"],
        "cancelled"
    );

    // A runner that stopped mid-job gives it back after the lease.
    ok(
        &mut e,
        Request::post("/api/workers", json!({"worker": "SendEmail", "id": "j2"})),
    );
    assert_eq!(e.claim_jobs(&["SendEmail"], 10, "node-1").len(), 1);
    advance(&mut e, 6 * 60 * 1000);
    e.tidy_effects(5 * 60 * 1000);
    assert_eq!(e.claim_jobs(&["SendEmail"], 10, "node-2").len(), 1);
}

#[test]
fn settings_hide_secrets_and_keep_them_when_the_mask_comes_back() {
    let mut e = engine();
    let s = ok(
        &mut e,
        Request::patch(
            "/api/settings",
            json!({"email_api_key": "re_123", "email_provider": "resend"}),
        ),
    );
    assert_eq!(s["email_api_key"], "********");
    assert_eq!(s["email_provider"], "resend");
    ok(
        &mut e,
        Request::patch("/api/settings", json!({"email_api_key": "********"})),
    );
    assert_eq!(e.settings()["email_api_key"], "re_123");
    let bad = e.handle(
        Request::patch("/api/settings", json!({"rate_limiting_rules": "[{"}))
            .header("X-Admin-Key", "admin"),
    );
    assert_eq!(bad.status, 400);
}

#[test]
fn oauth_sign_in_links_only_verified_emails() {
    let mut e = engine();
    ok(
        &mut e,
        Request::post(
            "/api/moul",
            json!({"name": "users", "type": "auth", "fields": []}),
        ),
    );
    ok(
        &mut e,
        Request::patch(
            "/api/settings",
            json!({"oauth_github_enabled": "true", "oauth_github_client_id": "cid", "oauth_github_client_secret": "sec"}),
        ),
    );
    let m = e.handle(Request::get("/api/moul/users/auth-methods"));
    let p = &m.json_body()["oauth2"]["providers"][0];
    assert_eq!(p["name"], "github");
    let url = p["authUrl"].as_str().unwrap();
    assert!(url.starts_with("https://github.com/login/oauth/authorize?client_id=cid"));
    assert!(url.contains(
        "redirect_uri=https%3A%2F%2Fapp.example%2Fapi%2Fmoul%2Fusers%2Foauth2%2Fgithub%2Fcallback"
    ));
    let state = e.verify_oauth_state(p["state"].as_str().unwrap()).unwrap();
    assert_eq!(state.collection, "users");
    assert_eq!(state.verifier, p["codeVerifier"]);

    let profile = ExternalProfile {
        provider: "github".into(),
        id: "42".into(),
        email: "dev@x.co".into(),
        email_verified: true,
        username: "dev-one".into(),
        ..ExternalProfile::default()
    };
    let first = e.sign_in_external("users", profile.clone(), None);
    assert_eq!(first.status, 200, "{:?}", first.json_body());
    let id = first.json_body()["record"]["id"].clone();
    assert_eq!(first.json_body()["record"]["username"], "devone");
    // The same provider id signs in to the same account.
    let again = e.sign_in_external("users", profile, None);
    assert_eq!(again.json_body()["record"]["id"], id);

    // An unverified email does not take over an existing account.
    e.handle(Request::post(
        "/api/moul/users/otp/request",
        json!({"email": "owner@x.co"}),
    ));
    let sneaky = e.sign_in_external(
        "users",
        ExternalProfile {
            provider: "google".into(),
            id: "g1".into(),
            email: "owner@x.co".into(),
            email_verified: false,
            ..ExternalProfile::default()
        },
        None,
    );
    assert_eq!(sneaky.status, 400, "{:?}", sneaky.json_body());

    // Someone signs up with a password and another person's address; the
    // address's owner, signing in with a provider, does not land in it.
    ok(
        &mut e,
        Request::patch("/api/moul/users", json!({"rules": {"createRule": ""}})),
    );
    let squat = e.handle(Request::post(
        "/api/moul/users/records",
        json!({"username": "squatter", "email": "victim@x.co", "password": "Passw0rdOK", "passwordConfirm": "Passw0rdOK"}),
    ));
    assert_eq!(squat.status, 201, "{:?}", squat.json_body());
    let victim = e.sign_in_external(
        "users",
        ExternalProfile {
            provider: "google".into(),
            id: "g-victim".into(),
            email: "victim@x.co".into(),
            email_verified: true,
            ..ExternalProfile::default()
        },
        None,
    );
    assert_eq!(victim.status, 400, "{:?}", victim.json_body());

    // An address the provider does not vouch for is not kept as the
    // account's.
    let unverified = e.sign_in_external(
        "users",
        ExternalProfile {
            provider: "github".into(),
            id: "77".into(),
            email: "later@x.co".into(),
            email_verified: false,
            ..ExternalProfile::default()
        },
        None,
    );
    assert_eq!(unverified.status, 200);
    assert_eq!(unverified.json_body()["record"]["email"], "77@github.oauth");
}
