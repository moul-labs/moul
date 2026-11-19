//! The dispatcher: sends the mail, makes the webhook calls and runs the
//! jobs that writes left as rows (see `moul_core::effects`).
//!
//! It wakes after every write and every five seconds, takes what is due
//! from the engine in small batches, does the network work off the engine's
//! thread, and reports each outcome back, so the engine retries failures
//! with backoff. Only the writer's server runs one; a read-only copy sends
//! nothing.
//!
//! Built-in jobs:
//!
//! | Worker | Arguments | Does |
//! | --- | --- | --- |
//! | `SendEmail` | `to` (comma-separated), `subject`, `body` | sends mail, as the Go engine's job did |
//! | `Webhook` | `url`, `payload`, `secret`? | POSTs `payload` as JSON, signed like a webhook |
//!
//! A job for any other worker waits for a program of the builder's to claim
//! it through `/api/workers`.

use std::collections::HashSet;
use std::time::Duration;

use futures_util::future::join_all;
use serde_json::Value;

use crate::http::AppState;
use crate::mail::{self, MailConfig, Message};
use crate::webhooks;

/// The workers this server runs.
pub const WORKERS: [&str; 2] = ["SendEmail", "Webhook"];

/// Starts the dispatcher.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut nudges = state.db.nudges();
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).ok();
        let node = format!("node-{}", moul_core::ids::hex(&b));
        let mut shown = HashSet::new();
        loop {
            tokio::select! {
                changed = nudges.changed() => if changed.is_err() { return },
                () = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            if !state.db.is_open() {
                return;
            }
            run_once(&state, &node, &mut shown).await;
        }
    });
}

/// One round: mail, webhook calls, jobs.
pub async fn run_once(state: &AppState, node: &str, shown: &mut HashSet<String>) {
    let settings = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mail_config = MailConfig::from_settings(&settings);

    if mail_config.sends() {
        let due = state
            .db
            .run(|db| db.engine.due_mail(20))
            .await
            .unwrap_or_default();
        let sends = due.into_iter().map(|m| {
            let (state, cfg) = (state.clone(), mail_config.clone());
            async move {
                let msg = Message {
                    to: vec![m.to.clone()],
                    subject: m.subject.clone(),
                    body: m.body.clone(),
                };
                let outcome = mail::send(&state.outbound, &cfg, &msg).await;
                if let Err(e) = &outcome {
                    eprintln!("moul: mail to {} failed (attempt {}): {e}", m.to, m.attempt);
                }
                let _ = state
                    .db
                    .run(move |db| db.engine.mail_done(&m.id, outcome))
                    .await;
            }
        });
        join_all(sends).await;
    } else {
        show_kept_mail(state, shown).await;
    }

    let calls = state
        .db
        .run(|db| db.engine.due_webhooks(20))
        .await
        .unwrap_or_default();
    let sends = calls.into_iter().map(|c| {
        let state = state.clone();
        async move {
            let done = webhooks::call(
                &state,
                webhooks::Call {
                    url: &c.url,
                    event: &c.event,
                    webhook: &c.webhook,
                    delivery: &c.id,
                    timestamp: &c.timestamp,
                    secret: c.secret.as_deref(),
                    payload: &c.payload,
                },
            )
            .await;
            let outcome = match done.error {
                None => Ok(()),
                Some(e) => Err(e),
            };
            let _ = state
                .db
                .run(move |db| db.engine.webhook_done(&c.id, outcome))
                .await;
        }
    });
    join_all(sends).await;

    let node = node.to_owned();
    let jobs = state
        .db
        .run(move |db| db.engine.claim_jobs(&WORKERS, 10, &node))
        .await
        .unwrap_or_default();
    let runs = jobs.into_iter().map(|j| {
        let (state, cfg) = (state.clone(), mail_config.clone());
        async move {
            let outcome = run_job(&state, &cfg, &j.worker, &j.args, &j.id).await;
            let _ = state
                .db
                .run(move |db| db.engine.job_done(&j.id, outcome))
                .await;
        }
    });
    join_all(runs).await;
}

async fn run_job(
    state: &AppState,
    cfg: &MailConfig,
    worker: &str,
    args: &Value,
    id: &str,
) -> Result<(), String> {
    let text = |k: &str| args[k].as_str().unwrap_or("").to_owned();
    match worker {
        "SendEmail" => {
            if !cfg.sends() {
                return Err("mail is not set up (email_enabled and email_provider)".into());
            }
            let to: Vec<String> = text("to")
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            if to.is_empty() {
                return Err("SendEmail needs `to`".into());
            }
            let mut cfg = cfg.clone();
            if !text("from").is_empty() {
                cfg.from_address = text("from");
            }
            if !text("from_name").is_empty() {
                cfg.from_name = text("from_name");
            }
            mail::send(
                &state.outbound,
                &cfg,
                &Message {
                    to,
                    subject: text("subject"),
                    body: text("body"),
                },
            )
            .await
        }
        "Webhook" => {
            let url = text("url");
            if url.is_empty() {
                return Err("Webhook needs `url`".into());
            }
            let payload = args
                .get("payload")
                .cloned()
                .unwrap_or(Value::Null)
                .to_string();
            let ts = moul_core::time::rfc3339(moul_vfs::system_clock());
            let secret = args["secret"].as_str();
            let done = webhooks::call(
                state,
                webhooks::Call {
                    url: &url,
                    event: "job",
                    webhook: "",
                    delivery: id,
                    timestamp: &ts,
                    secret,
                    payload: &payload,
                },
            )
            .await;
            done.error.map_or(Ok(()), Err)
        }
        other => Err(format!("no built-in worker {other}")),
    }
}

/// With mail off, prints each new mail once so its codes can be read.
async fn show_kept_mail(state: &AppState, shown: &mut HashSet<String>) {
    let inbox = state
        .db
        .run(|db| {
            db.engine
                .handle(moul_core::Request::get("/api/inbox").owner())
                .json_body()
                .clone()
        })
        .await
        .unwrap_or(Value::Null);
    let started = moul_core::time::rfc3339(
        moul_vfs::system_clock() - state.started.elapsed().as_millis() as i64,
    );
    let items = inbox["items"].as_array().cloned().unwrap_or_default();
    for m in items.iter().rev() {
        let id = m["id"].as_str().unwrap_or("").to_owned();
        if m["createdAt"].as_str().unwrap_or("") < started.as_str() || !shown.insert(id) {
            continue;
        }
        println!(
            "mail (not sent: mail is off) to {}: {}\n{}\n",
            m["to"].as_str().unwrap_or(""),
            m["subject"].as_str().unwrap_or(""),
            strip_tags(m["body"].as_str().unwrap_or(""))
        );
    }
    if shown.len() > 10_000 {
        shown.clear();
    }
}

/// Mail bodies are HTML; a terminal wants the text.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut inside = false;
    for c in html.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
